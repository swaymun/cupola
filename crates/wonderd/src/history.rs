//! Local transcript projection and independent runtime hydration.
use super::*;
use wonder_api::FILE_CHANGE_PROJECTION_VERSION;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationMessage {
    pub(super) message_id: String,
    pub(super) client_message_id: String,
    pub(super) original_body_sha256: Option<String>,
    pub(super) body: String,
    pub(super) state: String,
    pub(super) created_at: String,
    pub(super) body_sha256: String,
    pub(super) codex_thread_id: Option<String>,
    pub(super) codex_turn_id: Option<String>,
    pub(super) attachment_ids: Vec<String>,
    /// Set when another thread, or Wonder reporting finished tasks, wrote it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<wonder_store::MessageSource>,
    /// Why the Mac could not deliver it, in plain language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) delivery_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationAssistantMessage {
    pub(super) message_id: String,
    pub(super) codex_thread_id: String,
    pub(super) codex_turn_id: String,
    pub(super) item_id: String,
    pub(super) text: String,
    pub(super) state: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationSnapshot {
    pub(super) conversation_id: String,
    pub(super) host_epoch: String,
    pub(super) last_sequence: u64,
    pub(super) history_revision: i64,
    pub(super) codex_thread_id: Option<String>,
    pub(super) messages: Vec<ConversationMessage>,
    pub(super) assistant_messages: Vec<ConversationAssistantMessage>,
    pub(super) thread: ConversationThreadProjection,
    pub(super) events: Vec<HostEventEnvelope>,
    /// Every agent record in the conversation that is a message Cupola sent
    /// through T3 Code, as each item of `thread` already says: the phone
    /// labels a record it saved outside this page the same way.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) message_records: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationThreadProjection {
    pub(super) thread_id: Option<String>,
    pub(super) turns: Vec<ConversationTurn>,
    pub(super) next_cursor: Option<String>,
    pub(super) hydrated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationTurn {
    pub(super) started_at: Option<String>,
    pub(super) completed_at: Option<String>,
    pub(super) id: String,
    pub(super) status: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) items: Vec<ConversationThreadItem>,
    /// Running in a desktop or terminal app on the Mac; Wonder can't steer it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) running_elsewhere: bool,
    /// Every file the turn changed, from the provider's whole-turn diff
    /// (`paths`, `diffs`, `additions`, `deletions`, as on a `fileChange`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) edited_files: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationThreadItem {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) item_type: String,
    pub(super) state: String,
    pub(super) text: Option<String>,
    pub(super) payload: serde_json::Value,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    #[serde(skip)]
    pub(super) source_rank: u8,
    /// Lifecycle authority is separate from source rank: a richer hydrated
    /// payload must not turn a known-running status-less item into completed.
    #[serde(skip)]
    pub(super) lifecycle_authority: u8,
}

#[derive(Clone, Debug)]
pub(super) struct AppServerThreadItem {
    pub(super) turn_id: String,
    pub(super) item: serde_json::Value,
}

pub(super) fn thread_item_type_for_activity(category: &str) -> (&'static str, Option<String>) {
    match category.to_ascii_lowercase().as_str() {
        "command" => ("commandExecution", None),
        "file change" | "file" => ("fileChange", None),
        "mcp tool" | "tool" => ("mcpToolCall", None),
        "search" | "web search" => ("webSearch", None),
        "plan" => ("plan", None),
        "reasoning" => ("reasoning", None),
        "compaction" | "context compaction" => ("contextCompaction", None),
        "collaboration" | "collab" => ("collabAgentToolCall", None),
        "image" | "image view" => ("imageView", None),
        "image generation" | "generated image" => ("imageGeneration", None),
        "approval" => ("approval", None),
        "error" => ("error", None),
        _ => ("unknown", Some(category.to_owned())),
    }
}

pub(super) fn thread_activity_payload(
    category: &str,
    detail: Option<&String>,
) -> serde_json::Value {
    if let Some(detail) = detail {
        if let Ok(serde_json::Value::Object(mut object)) =
            serde_json::from_str::<serde_json::Value>(detail)
        {
            object
                .entry("category")
                .or_insert_with(|| serde_json::Value::String(category.to_owned()));
            return serde_json::Value::Object(object);
        }
    }
    let detail_value = detail
        .cloned()
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);
    match category.to_ascii_lowercase().as_str() {
        "command" => serde_json::json!({ "category": category, "command": detail_value }),
        "file change" | "file" => serde_json::json!({
            "category": category,
            "paths": detail.map(String::as_str).unwrap_or("").split(", ").filter(|path| !path.is_empty()).collect::<Vec<_>>(),
        }),
        "search" | "web search" => {
            serde_json::json!({ "category": category, "query": detail_value })
        }
        "compaction" | "context compaction" => {
            serde_json::json!({ "category": category, "summary": detail_value })
        }
        "mcp tool" | "tool" => serde_json::json!({ "category": category, "tool": detail_value }),
        _ => serde_json::json!({ "category": category, "detail": detail_value }),
    }
}

/// Compare native RFC3339 and journal millisecond timestamps by their instant,
/// retaining nanoseconds. Equivalent spellings share a key so stable sorts
/// preserve observed order. Unreadable timestamps retain the store's ordering:
/// before readable timestamps, and lexically among themselves.
fn history_timestamp_key(value: &str) -> (Option<i128>, String) {
    let text = value.trim();
    let instant = text
        .parse::<i128>()
        .ok()
        .and_then(|ms| ms.checked_mul(1_000_000))
        .or_else(|| {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|stamp| stamp.unix_timestamp_nanos())
        });
    (
        instant,
        if instant.is_some() {
            String::new()
        } else {
            value.to_owned()
        },
    )
}

pub(super) fn ensure_conversation_turn(
    turns: &mut HashMap<String, ConversationTurn>,
    order: &mut Vec<String>,
    turn_id: &str,
    created_at: &str,
    updated_at: &str,
) {
    if !turns.contains_key(turn_id) {
        order.push(turn_id.to_owned());
        turns.insert(
            turn_id.to_owned(),
            ConversationTurn {
                started_at: None,
                completed_at: None,
                id: turn_id.to_owned(),
                status: "unknown".into(),
                created_at: created_at.to_owned(),
                updated_at: updated_at.to_owned(),
                items: Vec::new(),
                running_elsewhere: false,
                edited_files: None,
            },
        );
    }
    if let Some(turn) = turns.get_mut(turn_id) {
        if history_timestamp_key(created_at) < history_timestamp_key(&turn.created_at) {
            turn.created_at = created_at.to_owned();
        }
        if history_timestamp_key(updated_at) > history_timestamp_key(&turn.updated_at) {
            turn.updated_at = updated_at.to_owned();
        }
    }
}

pub(super) fn thread_item_state(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "started" => "started",
        "working" | "running" | "streaming" | "updated" | "inprogress" | "in_progress"
        | "in-progress" => "streaming",
        "completed" | "complete" | "available" => "completed",
        "failed" | "error" => "failed",
        "interrupted" | "aborted" => "interrupted",
        "waiting" | "pending" => "waiting",
        _ => "unknown",
    }
}

pub(super) fn thread_item_state_rank(state: &str) -> u8 {
    match state {
        "unknown" => 0,
        "started" => 1,
        "waiting" => 2,
        "streaming" => 3,
        "completed" | "failed" | "interrupted" => 4,
        _ => 0,
    }
}

pub(super) fn upsert_conversation_thread_item(
    turn: &mut ConversationTurn,
    incoming: ConversationThreadItem,
) {
    upsert_conversation_thread_item_at(turn, incoming, 1);
}

pub(super) fn upsert_durable_thread_item(
    turn: &mut ConversationTurn,
    incoming: ConversationThreadItem,
) {
    upsert_conversation_thread_item_at(turn, incoming, 2);
}

pub(super) fn upsert_conversation_thread_item_at(
    turn: &mut ConversationTurn,
    mut incoming: ConversationThreadItem,
    source_rank: u8,
) {
    incoming.source_rank = source_rank;
    let Some(index) = turn.items.iter().position(|item| item.id == incoming.id) else {
        turn.items.push(incoming);
        return;
    };
    let existing = &mut turn.items[index];
    let thinking_timing = existing.payload.get("thinkingTiming").cloned();
    let task_payload =
        crate::task_checklist::merge_projection(&existing.payload, &incoming.payload);
    let durable_created_at =
        if existing.item_type == "agentMessage" && incoming.item_type == "agentMessage" {
            if incoming.source_rank == 2 {
                Some(incoming.created_at.clone())
            } else if existing.source_rank == 2 {
                Some(existing.created_at.clone())
            } else {
                None
            }
        } else {
            None
        };
    let preserve_payload_state_tiebreaker =
        existing.item_type != "contextCompaction" && incoming.item_type != "contextCompaction";
    let updated_order = history_timestamp_key(&incoming.updated_at)
        .cmp(&history_timestamp_key(&existing.updated_at));
    let incoming_rank = thread_item_state_rank(&incoming.state);
    let existing_rank = thread_item_state_rank(&existing.state);
    let replace_payload = incoming.source_rank > existing.source_rank
        || (incoming.source_rank == existing.source_rank
            && if preserve_payload_state_tiebreaker {
                incoming_rank > existing_rank
                    || (incoming_rank == existing_rank && updated_order.is_ge())
            } else {
                updated_order.is_ge()
            });
    // Durable text has no phase column. Keep actual lifecycle metadata instead
    // of interpreting every completed assistant item as a final answer.
    let phase = if existing.item_type == "agentMessage" && incoming.item_type == "agentMessage" {
        let (preferred, fallback) = if replace_payload {
            (&incoming.payload, &existing.payload)
        } else {
            (&existing.payload, &incoming.payload)
        };
        preferred
            .get("phase")
            .filter(|v| !v.is_null())
            .or_else(|| fallback.get("phase").filter(|v| !v.is_null()))
            .cloned()
    } else {
        None
    };
    if replace_payload {
        existing.source_rank = incoming.source_rank;
        existing.item_type = incoming.item_type.clone();
        if incoming.text.is_some() {
            existing.text = incoming.text.clone();
        }
        existing.payload = incoming.payload.clone();
    }
    if let Some(payload) = task_payload {
        existing.payload["taskChecklist"] = payload["taskChecklist"].clone();
        if let Some(stamp) = payload.get("updatedAt") {
            existing.payload["updatedAt"] = stamp.clone();
        }
    }
    if existing.item_type == "reasoning" {
        if let Some(timing) = thinking_timing {
            existing.payload["thinkingTiming"] = timing;
        }
    }
    if let (Some(phase), Some(payload)) = (phase, existing.payload.as_object_mut()) {
        payload.insert("phase".into(), phase);
    }
    let observed_lifecycle = matches!(
        existing.item_type.as_str(),
        "contextCompaction" | "reasoning"
    ) || matches!(
        incoming.item_type.as_str(),
        "contextCompaction" | "reasoning"
    );
    // Keep the pre-existing monotonic rank behavior for every ordinary item:
    // a reconnect may hydrate a completed command after item/started was seen.
    // Status-less compaction/reasoning must not infer completion over an
    // explicitly observed native lifecycle.
    let lifecycle_wins = if observed_lifecycle {
        incoming.lifecycle_authority > existing.lifecycle_authority
            || (incoming.lifecycle_authority == existing.lifecycle_authority
                && (incoming_rank > existing_rank
                    || (incoming_rank == existing_rank && updated_order.is_ge())))
    } else {
        incoming_rank > existing_rank || (incoming_rank == existing_rank && updated_order.is_ge())
    };
    if existing_rank < 4 && lifecycle_wins {
        existing.state = incoming.state;
        existing.lifecycle_authority = incoming.lifecycle_authority;
    }
    if history_timestamp_key(&incoming.created_at) < history_timestamp_key(&existing.created_at) {
        existing.created_at = incoming.created_at;
    }
    if let Some(timestamp) = durable_created_at {
        existing.created_at = timestamp;
    }
    if updated_order.is_gt() {
        existing.updated_at = incoming.updated_at;
    }
}

// A matched pair of observed native item events is the only clock evidence.
// Hydration timestamps and turn duration never enter this measurement.
fn observed_thinking_timing(
    previous: Option<&serde_json::Value>,
    state: &str,
    event: &HostEventEnvelope,
) -> serde_json::Value {
    let unknown = serde_json::json!({"finished": true});
    if event.sequence > 9_007_199_254_740_991 {
        return unknown;
    }
    if previous
        .and_then(|v| v.get("finished"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return previous.cloned().unwrap_or(unknown);
    }
    let at = event
        .occurred_at
        .parse::<i64>()
        .ok()
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(&event.occurred_at)
                .ok()
                .map(|v| v.timestamp_millis())
        })
        .filter(|v| (1..=9_007_199_254_740_991).contains(v));
    let Some(at) = at else {
        return unknown;
    };
    if state == "started" {
        return previous.cloned().unwrap_or_else(|| {
            serde_json::json!({
                "startedAtMs": at, "hostEpoch": event.host_epoch, "startSequence": event.sequence
            })
        });
    }
    if state != "completed" {
        return unknown;
    }
    let Some(previous) = previous else {
        return unknown;
    };
    let Some(start) = previous
        .get("startedAtMs")
        .and_then(serde_json::Value::as_i64)
    else {
        return unknown;
    };
    let matches = previous
        .get("hostEpoch")
        .and_then(serde_json::Value::as_str)
        == Some(event.host_epoch.as_str())
        && previous
            .get("startSequence")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|sequence| sequence < event.sequence)
        && (0..=86_400_000).contains(&(at - start));
    if !matches {
        return unknown;
    }
    let mut timing = previous.clone();
    timing["completedAtMs"] = at.into();
    timing["finished"] = true.into();
    timing
}

pub(super) fn known_thread_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "userMessage"
            | "hookPrompt"
            | "agentMessage"
            | "functionCallOutput"
            | "reasoning"
            | "commandExecution"
            | "fileChange"
            | "turnDiff"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "subAgentActivity"
            | "webSearch"
            | "plan"
            | "promptSuggestion"
            | "taskChecklist"
            | "imageView"
            | "imageGeneration"
            | "sleep"
            | "enteredReviewMode"
            | "exitedReviewMode"
            | "collabAgentToolCall"
            | "contextCompaction"
            | "approval"
            | "error"
            | "providerSwitch"
            | "taskNotification"
    )
}

pub(super) fn typed_item_timestamp(item: &serde_json::Value, fallback: &str) -> String {
    item.get("createdAt")
        .or_else(|| item.get("created_at"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub(super) fn item_state_from_value(item: &serde_json::Value) -> &'static str {
    item.get("status")
        .or_else(|| item.get("state"))
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("type").and_then(serde_json::Value::as_str))
        })
        .map(thread_item_state)
        .unwrap_or_else(
            || match item.get("type").and_then(serde_json::Value::as_str) {
                // ContextCompactionThreadItem is intentionally status-less;
                // completion requires an observed item lifecycle, never the
                // containing turn's eventual outcome.
                Some("contextCompaction" | "reasoning") => "unknown",
                Some(
                    "agentMessage" | "plan" | "commandExecution" | "fileChange" | "mcpToolCall"
                    | "dynamicToolCall",
                ) => "completed",
                Some(_) => "completed",
                None => "unknown",
            },
        )
}

// Provider messages have no declared text limit. Keep replies well above the
// 64 KiB send limit while bounding each text field and the combined display text.
const MESSAGE_TEXT_BYTES: usize = 1024 * 1024;
const MESSAGE_TRUNCATED: &str = "\n\n[Message truncated: too long to display]";

pub(super) fn sanitize_ansi_and_secrets(value: &str, limit: usize) -> String {
    sanitize_text(value, limit, false)
}

fn sanitize_text(value: &str, limit: usize, message: bool) -> String {
    let protected = crate::secrets::redact_text(value);
    let value = protected.as_str();
    let mut clean = String::with_capacity(value.len().min(limit));
    let mut chars = value.chars().peekable();
    let mut truncated = false;
    while let Some(ch) = chars.next() {
        // Terminal strings (titles, hyperlinks, clipboard/image commands) are
        // controls, not output. Removing ESC alone leaks their payload as text.
        // Accept the seven-bit and C1 encodings; an unfinished control stays
        // hidden to the end of its line until a streamed projection includes its terminator.
        let control = if ch == '\u{1b}' {
            chars.next_if(|c| matches!(c, '[' | ']' | 'P' | 'X' | '^' | '_' | '\\'))
        } else {
            match ch {
                '\u{9b}' => Some('['),
                '\u{9d}' => Some(']'),
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Some('P'),
                '\u{9c}' => Some('\\'),
                _ => None,
            }
        };
        if control == Some('[') {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) || matches!(next, '\u{18}' | '\u{1a}') {
                    break;
                }
            }
            continue;
        }
        if matches!(control, Some(']' | 'P' | 'X' | '^' | '_')) {
            // Real payloads (titles, links, image data) are one line. A control
            // that never terminates ends at the line break, so one stray
            // introducer cannot hide the rest of a finished message.
            while let Some(next) = chars.next_if(|c| *c != '\n') {
                if matches!(next, '\u{9c}' | '\u{18}' | '\u{1a}')
                    || (control == Some(']') && next == '\u{7}')
                    || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                {
                    break;
                }
            }
            continue;
        }
        if ch == '\u{1b}' && control.is_none() {
            // Character-set selection (ESC ( B), cursor save (ESC 7), etc.
            // Consume the escape's intermediates and final byte, never printed
            // backslash spellings such as "\\x1b[32m" in an owner's example.
            while chars.next_if(|c| (' '..='/').contains(c)).is_some() {}
            chars.next_if(|c| ('0'..='~').contains(c));
            continue;
        }
        if ch == '\u{1b}' || control.is_some() || ch == '\u{7}' {
            continue;
        }
        if clean.len() + ch.len_utf8() > limit {
            truncated = true;
            break;
        }
        clean.push(ch);
    }
    for prefix in ["sk-", "ghp_", "github_pat_", "xoxb-", "Bearer ", "token="] {
        let mut offset = 0;
        while let Some(found) = clean[offset..].find(prefix) {
            let start = offset + found;
            // Key prefixes start a token; "sk-" inside a word such as
            // "task-id" or "disk-usage" is ordinary text.
            if prefix != "token="
                && clean[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                offset = start + prefix.len();
                if offset >= clean.len() {
                    break;
                }
                continue;
            }
            let end = clean[start + prefix.len()..]
                .find(char::is_whitespace)
                .map(|idx| start + prefix.len() + idx)
                .unwrap_or(clean.len());
            clean.replace_range(start..end, "[redacted]");
            offset = start + "[redacted]".len();
            if offset >= clean.len() {
                break;
            }
        }
    }
    if message && (truncated || clean.len() > limit) {
        let mut end = limit
            .saturating_sub(MESSAGE_TRUNCATED.len())
            .min(clean.len());
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
        clean.push_str(MESSAGE_TRUNCATED);
    }
    clean
}

/// Codex's complete, trailing memory attribution has human notes plus private
/// bookkeeping. Present the notes, not XML or rollout IDs. This is applied only
/// to assistant display text; user pastes and code examples are never classified
/// as metadata. Unknown/malformed shapes stay verbatim rather than losing text.
fn render_memory_citations(text: String) -> String {
    let Some((body, trailer)) = text.rsplit_once("\n<oai-mem-citation>\n") else {
        return text;
    };
    // A trailer inside an unfinished fenced example is still example text.
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let Some(marker @ ('`' | '~')) = trimmed.chars().next() else {
            continue;
        };
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if count < 3 {
            continue;
        }
        let rest = &trimmed[count..];
        match fence {
            Some((open, width)) if marker == open && count >= width && rest.trim().is_empty() => {
                fence = None
            }
            None if marker != '`' || !rest.contains('`') => fence = Some((marker, count)),
            _ => {}
        }
    }
    if fence.is_some() {
        return text;
    }
    let Some(entries) = trailer
        .trim_end()
        .strip_prefix("<citation_entries>\n")
        .and_then(|s| s.strip_suffix("\n</rollout_ids>\n</oai-mem-citation>"))
    else {
        return text;
    };
    let Some((entries, ids)) = entries.split_once("\n</citation_entries>\n<rollout_ids>\n") else {
        return text;
    };
    if !ids
        .lines()
        .all(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
    {
        return text;
    }
    let mut notes = Vec::new();
    for entry in entries.lines() {
        let Some((source, note)) = entry.split_once("|note=[") else {
            return text;
        };
        let Some(note) = note.strip_suffix(']') else {
            return text;
        };
        let valid_source = source.rsplit_once(':').is_some_and(|(path, lines)| {
            !path.is_empty()
                && lines.split_once('-').is_some_and(|(start, end)| {
                    start.parse::<u32>().is_ok_and(|n| n > 0)
                        && end.parse::<u32>().is_ok_and(|n| n > 0)
                })
        });
        if !valid_source || note.trim().is_empty() {
            return text;
        }
        notes.push(format!("- {note}"));
    }
    if notes.is_empty() {
        return text;
    }
    format!(
        "{}\n\nMemory sources:\n{}",
        body.trim_end(),
        notes.join("\n")
    )
}

fn assistant_display_text(text: &str) -> String {
    render_memory_citations(sanitize_text(text, MESSAGE_TEXT_BYTES, true))
}

fn is_message_item(item: &serde_json::Value) -> bool {
    matches!(
        item.get("type").and_then(serde_json::Value::as_str),
        Some("agentMessage" | "userMessage")
    )
}

/// The longest thinking summary part, and the most parts, a reasoning item keeps.
const THINKING_SUMMARY_PART_BYTES: usize = 4 * 1024;
const THINKING_SUMMARY_PARTS: usize = 16;

/// A reasoning item's summary parts (`summary: [String]`, or older
/// `[{"text": ...}]`), redacted and bounded. Never the raw `content`.
///
/// Each part is redacted whole before it is cut to size, so the cut cannot
/// split a credential into text redaction no longer recognises.
fn reasoning_summary(item: &serde_json::Value) -> Vec<String> {
    item.get("summary")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part.as_str()
                .or_else(|| part.get("text").and_then(serde_json::Value::as_str))
        })
        .map(|text| {
            let mut clean = sanitize_ansi_and_secrets(text.trim(), usize::MAX);
            let mut end = clean.len().min(THINKING_SUMMARY_PART_BYTES);
            while !clean.is_char_boundary(end) {
                end -= 1;
            }
            clean.truncate(end);
            clean.trim_end().to_owned()
        })
        .filter(|text| !text.is_empty())
        .take(THINKING_SUMMARY_PARTS)
        .collect()
}

/// Removes thinking text from a projected thread, keeping each thinking row's
/// lifecycle and observed timing. Used unless the provider shows thinking.
pub(super) fn hide_thinking(thread: &mut ConversationThreadProjection) {
    for item in thread
        .turns
        .iter_mut()
        .flat_map(|turn| turn.items.iter_mut())
        .filter(|item| item.item_type == "reasoning")
    {
        item.text = None;
        if let Some(payload) = item.payload.as_object_mut() {
            payload.remove("summary");
        }
    }
}

/// The same for one journal event: a thinking item's live upsert loses its
/// summary and display text.
pub(super) fn hide_thinking_event(event: &mut HostEventEnvelope) {
    let WonderEvent::Activity {
        category,
        detail: Some(detail),
        ..
    } = &mut event.event
    else {
        return;
    };
    if category != "thread_item_upsert" {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return;
    };
    let Some(item) = value
        .get_mut("item")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
        return;
    }
    item.remove("summary");
    if let Some(object) = value.as_object_mut() {
        object.remove("text");
    }
    *detail = value.to_string();
}

/// Thinking text reaches the phone only while the conversation shows the
/// Thinking detail level: its own override, else its provider's. An unknown
/// conversation or an unreadable setting hides it.
pub(super) async fn shows_thinking(state: &AppState, conversation: &str) -> bool {
    let Ok(family) = crate::claude::conversation_family(state, conversation).await else {
        return false;
    };
    state
        .store
        .effective_chat_detail(conversation, family)
        .await
        .is_ok_and(|detail| detail == wonder_store::ChatDetail::Thinking)
}

/// `hide_thinking_event` for each event whose conversation does not show
/// thinking. Reads settings only for reasoning upserts.
pub(super) async fn hide_thinking_events(state: &AppState, events: &mut [HostEventEnvelope]) {
    let mut shown = HashMap::new();
    for event in events {
        let WonderEvent::Activity {
            category,
            detail: Some(detail),
            ..
        } = &event.event
        else {
            continue;
        };
        if category != "thread_item_upsert" || !detail.contains("\"reasoning\"") {
            continue;
        }
        let show = match event.conversation_id.as_deref() {
            Some(id) => match shown.get(id) {
                Some(show) => *show,
                None => {
                    let show = shows_thinking(state, id).await;
                    shown.insert(id.to_owned(), show);
                    show
                }
            },
            None => false,
        };
        if !show {
            hide_thinking_event(event);
        }
    }
}

// Both the live item event and a history page use this same display projection.
pub(super) fn typed_item_display_text(item: &serde_json::Value) -> Option<String> {
    if item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning") {
        let summary = reasoning_summary(item);
        return (!summary.is_empty()).then(|| summary.join("\n\n"));
    }
    let message = is_message_item(item);
    extract_text(item).map(|text| {
        if item.get("type").and_then(serde_json::Value::as_str) == Some("agentMessage") {
            return assistant_display_text(&text);
        }
        let user = item.get("type").and_then(serde_json::Value::as_str) == Some("userMessage");
        let text = if user {
            crate::injected_context::owner_text(&text)
        } else {
            text
        };
        sanitize_text(
            &text,
            if message {
                MESSAGE_TEXT_BYTES
            } else {
                16 * 1024
            },
            message,
        )
    })
}

pub(super) fn sanitize_path_value(value: &str, workspace: Option<&str>) -> String {
    // Codex names some files by URL (an imageView's `file:///…`); it is a
    // path on the Mac all the same.
    if let Some(url) = value.strip_prefix("file://") {
        return match crate::media_refs::percent_decode(url) {
            Some(path) if FsPath::new(&path).is_absolute() => sanitize_path_value(&path, workspace),
            _ => "[path outside workspace]".into(),
        };
    }
    if !FsPath::new(value).is_absolute() {
        return value.to_owned();
    }
    if workspace.is_some_and(|root| FsPath::new(value) == FsPath::new(root)) {
        return ".".into();
    }
    workspace
        .and_then(|root| sanitized_relative_path(value, root))
        .unwrap_or_else(|| "[path outside workspace]".into())
}

pub(super) fn sensitive_projection_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    normalized == "env"
        || normalized.contains("environment")
        || normalized.contains("header")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized.contains("cookie")
        || normalized.contains("approvalpayload")
        || normalized == "method"
}

pub(super) fn secret_projection_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("apikey")
        || normalized.contains("password")
}

pub(super) fn projection_path_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    normalized == "cwd" || normalized.ends_with("path")
}

pub(super) fn projection_paths_key(key: &str) -> bool {
    key.to_ascii_lowercase()
        .replace(['_', '-'], "")
        .ends_with("paths")
}

pub(super) fn sanitize_json_value(
    value: &mut serde_json::Value,
    limit: usize,
    workspace: Option<&str>,
    message_text: bool,
) {
    match value {
        serde_json::Value::String(text) => {
            *text = sanitize_text(
                text,
                if message_text {
                    MESSAGE_TEXT_BYTES
                } else {
                    limit
                },
                message_text,
            );
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_json_value(value, limit, workspace, message_text);
            }
        }
        serde_json::Value::Object(values) => {
            values.retain(|key, _| !sensitive_projection_key(key));
            for (key, value) in values.iter_mut() {
                if secret_projection_key(key) {
                    *value = serde_json::Value::String("[redacted]".into());
                    continue;
                }
                if projection_path_key(key) {
                    if let Some(text) = value.as_str() {
                        *value = serde_json::Value::String(sanitize_path_value(
                            &sanitize_ansi_and_secrets(text, limit),
                            workspace,
                        ));
                        continue;
                    }
                }
                if projection_paths_key(key) {
                    if let Some(paths) = value.as_array_mut() {
                        for path in paths {
                            if let Some(text) = path.as_str() {
                                *path = serde_json::Value::String(sanitize_path_value(
                                    &sanitize_ansi_and_secrets(text, limit),
                                    workspace,
                                ));
                            } else {
                                sanitize_json_value(path, limit, workspace, false);
                            }
                        }
                        continue;
                    }
                }
                // Only message text containers get the larger ceiling; tool,
                // image, path and other metadata strings keep their own limits.
                let text = message_text
                    && matches!(
                        key.as_str(),
                        "text" | "content" | "parts" | "message" | "output" | "delta"
                    );
                sanitize_json_value(value, limit, workspace, text);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
pub(super) fn sanitize_typed_item(
    item: &serde_json::Value,
    workspace: Option<&str>,
) -> serde_json::Value {
    sanitize_typed_item_in_state(item, workspace, item_state_from_value(item))
}

/// `sanitize_typed_item` for an item whose lifecycle `state` was observed
/// outside the item itself (a native notification or the producer's clocks).
pub(super) fn sanitize_typed_item_in_state(
    item: &serde_json::Value,
    workspace: Option<&str>,
    state: &str,
) -> serde_json::Value {
    let message = is_message_item(item);
    let mut value = item.clone();
    crate::secrets::redact_item(&mut value);
    crate::task_checklist::sanitize(&mut value);
    // Thinking is private. Its native lifecycle always reaches the phone. This
    // is the only place a provider summary enters a row: only once the thinking
    // has completed (redacted whole, then bounded), and the serving boundary
    // shows it only at the Thinking detail level (`hide_thinking`). Running or
    // status-less thinking carries none. Raw reasoning content never leaves the
    // Mac, and provider payloads cannot supply Wonder's observed timing metadata.
    if item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning") {
        let summary = if thread_item_state(state) == "completed" {
            reasoning_summary(item)
        } else {
            Vec::new()
        };
        if let Some(object) = value.as_object_mut() {
            let position = object.get("t3Position").and_then(serde_json::Value::as_i64);
            object.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "id" | "type" | "status" | "state" | "createdAt"
                )
            });
            if let Some(position) = position {
                object.insert("t3Position".into(), position.into());
            }
            if !summary.is_empty() {
                object.insert("summary".into(), summary.into());
            }
        }
        return value;
    }

    if let Some(object) = value.as_object_mut() {
        let item_type = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        // These describe our serving boundary, never provider-supplied data.
        object.remove("outputBytes");
        object.remove("outputTruncated");
        for key in [
            "command",
            "cwd",
            "aggregatedOutput",
            "output",
            "arguments",
            "result",
            "path",
        ] {
            if key == "output" && message {
                continue; // Older message shapes put their text in output.
            }
            if let Some(raw) = object.get_mut(key) {
                if let Some(text) = raw.as_str() {
                    let mut safe = sanitize_ansi_and_secrets(
                        text,
                        if key == "aggregatedOutput" || key == "output" {
                            64 * 1024
                        } else {
                            16 * 1024
                        },
                    );
                    if key == "cwd" || key == "path" {
                        safe = sanitize_path_value(&safe, workspace);
                    }
                    *raw = serde_json::Value::String(safe);
                } else if key == "arguments" || key == "result" {
                    sanitize_json_value(raw, 16 * 1024, workspace, false);
                    if serde_json::to_vec(raw).is_ok_and(|bytes| bytes.len() > 16 * 1024) {
                        // Computer-use identity is native metadata, independent
                        // of a potentially huge accessibility tree or tool text.
                        let surface = (key == "result"
                            && item.get("server").and_then(serde_json::Value::as_str)
                                == Some("cua_repl"))
                        .then(|| bounded_computer_surface(raw))
                        .flatten();
                        *raw = if let Some(surface) = surface {
                            serde_json::json!({"_meta": {"codex/toolSurface": surface},
                                "content": [{"type": "text", "text": "[tool result truncated]"}]})
                        } else {
                            serde_json::Value::String("[tool result truncated]".into())
                        };
                    }
                }
            }
        }
        // Only provider-declared action metadata reaches the compact native label.
        // Keep this bounded separately from the full command/input disclosure.
        if let Some(action) = object.remove("toolAction") {
            if let Some(kind) = action
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .filter(|kind| {
                    matches!(
                        *kind,
                        "read"
                            | "listFiles"
                            | "search"
                            | "command"
                            | "edit"
                            | "webSearch"
                            | "fetch"
                            | "think"
                            | "changeMode"
                            | "ask"
                            | "updateTasks"
                            | "createTask"
                            | "readTasks"
                            | "stopTask"
                            | "findTools"
                            | "spawnAgent"
                    )
                })
            {
                let mut safe = serde_json::json!({"kind":kind});
                for key in ["query", "description", "url"] {
                    if let Some(text) = action.get(key).and_then(serde_json::Value::as_str) {
                        safe[key] = serde_json::Value::String(sanitize_ansi_and_secrets(text, 512));
                    }
                }
                if let Some(paths) = action.get("paths").and_then(serde_json::Value::as_array) {
                    if paths.len() > 32
                        || action
                            .get("pathsTruncated")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                        || paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .any(|path| path.len() > 512)
                    {
                        safe["pathsTruncated"] = serde_json::Value::Bool(true);
                    }
                    safe["paths"] = serde_json::Value::Array(
                        paths
                            .iter()
                            .take(32)
                            .filter_map(|path| path.as_str())
                            .map(|path| {
                                serde_json::Value::String(sanitize_path_value(
                                    &sanitize_ansi_and_secrets(path, 512),
                                    workspace,
                                ))
                            })
                            .collect(),
                    );
                }
                object.insert("toolAction".into(), safe);
            }
        }
        if item_type == "userMessage" {
            // Context handed over after a provider switch and Cupola's
            // preamble are the agent's briefing, and the phone signal is
            // Wonder's; none is something the owner wrote. A briefing that
            // opens the owner's own text (a thread started in T3 Code) leaves
            // that text.
            if let Some(parts) = object
                .get_mut("content")
                .and_then(serde_json::Value::as_array_mut)
            {
                parts.retain_mut(|part| {
                    let Some(text) = part.get("text").and_then(serde_json::Value::as_str) else {
                        return true;
                    };
                    if crate::wonder_skills::is_phone_context(text) {
                        return false;
                    }
                    let rest = if crate::provider_switch::is_handoff_text(text) {
                        crate::provider_switch::without_handoff(text).trim_start()
                    } else {
                        text
                    };
                    // What a harness wrapped around the owner's words (T3
                    // mention markup and context blocks, reminders, command
                    // tags) is for the agent; the provider still gets it.
                    let shown = crate::injected_context::owner_text(rest);
                    // Dropped when stripping left nothing; a part the owner
                    // sent blank stays as it was.
                    if shown.trim().is_empty() && !text.trim().is_empty() {
                        return false;
                    }
                    if shown != text {
                        part["text"] = serde_json::Value::String(shown);
                    }
                    true
                });
            }
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                let shown = crate::injected_context::owner_text(text);
                if shown != text {
                    object.insert("text".into(), serde_json::Value::String(shown));
                }
            }
        }
        if item_type == "commandExecution" {
            if let Some(output) = object.get_mut("aggregatedOutput") {
                if let Some(text) = output.as_str() {
                    *output = serde_json::Value::String(sanitize_ansi_and_secrets(text, 64 * 1024));
                }
            }
            if let Some(output) = object.remove("aggregatedOutput") {
                object.insert("output".into(), output);
            }
            if object
                .get("commandActions")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|actions| {
                    actions.len() > 32
                        || actions.iter().any(|action| {
                            action
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|path| path.len() > 512)
                        })
                })
            {
                object.insert(
                    "commandActionsTruncated".into(),
                    serde_json::Value::Bool(true),
                );
            }
            if let Some(actions) = object.get_mut("commandActions") {
                *actions = serde_json::Value::Array(
                    actions
                        .as_array()
                        .map(|actions| {
                            actions
                                .iter()
                                .take(32)
                                .filter_map(|action| {
                                    let kind = action.get("type")?.as_str()?;
                                    if !matches!(kind, "read" | "listFiles" | "search" | "unknown")
                                    {
                                        return None;
                                    }
                                    let mut safe = serde_json::json!({"type":kind});
                                    for key in ["path", "query"] {
                                        if let Some(text) =
                                            action.get(key).and_then(serde_json::Value::as_str)
                                        {
                                            let text = sanitize_ansi_and_secrets(text, 512);
                                            safe[key] =
                                                serde_json::Value::String(if key == "path" {
                                                    sanitize_path_value(&text, workspace)
                                                } else {
                                                    text
                                                });
                                        }
                                    }
                                    Some(safe)
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            object.remove("processId");
        }
        if matches!(item_type.as_str(), "fileChange" | "turnDiff")
            && object.get("changes").is_some()
        {
            let mut paths = Vec::new();
            let mut diffs = Vec::new();
            let mut additions = 0usize;
            let mut deletions = 0usize;
            if let Some(changes) = object.get("changes").and_then(serde_json::Value::as_array) {
                for change in changes {
                    let moved_to = change
                        .get("kind")
                        .filter(|kind| kind["type"] == "update")
                        .and_then(|kind| kind.get("move_path"))
                        .and_then(serde_json::Value::as_str);
                    let original_path = moved_to.and_then(|_| change.get("path"));
                    let path = moved_to
                        .or_else(|| change.get("path").and_then(serde_json::Value::as_str))
                        .map(|path| {
                            sanitize_path_value(
                                &sanitize_ansi_and_secrets(path, 16 * 1024),
                                workspace,
                            )
                        });
                    if let Some(path) = &path {
                        paths.push(path.clone());
                    }
                    let diff = change
                        .get("diff")
                        .or_else(|| change.get("unified_diff"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let kind = change
                        .get("kind")
                        .and_then(|kind| {
                            kind.as_str()
                                .or_else(|| kind.get("type").and_then(serde_json::Value::as_str))
                        })
                        .unwrap_or("update");
                    let raw_content = matches!(kind, "add" | "delete")
                        && !diff.lines().any(|line| line.starts_with("@@ "));
                    let added = if raw_content {
                        if kind == "add" {
                            diff.lines().count()
                        } else {
                            0
                        }
                    } else {
                        diff.lines()
                            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                            .count()
                    };
                    let removed = if raw_content {
                        if kind == "delete" {
                            diff.lines().count()
                        } else {
                            0
                        }
                    } else {
                        diff.lines()
                            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                            .count()
                    };
                    let safe = sanitize_ansi_and_secrets(diff, 16 * 1024);
                    let display = if raw_content {
                        safe.lines()
                            .map(|line| format!("{}{line}", if kind == "add" { '+' } else { '-' }))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        safe
                    };
                    let mut entry = serde_json::json!({"path": path, "diff": display, "kind": kind, "additions": added, "deletions": removed});
                    if let Some(original_path) = original_path {
                        entry["originalPath"] = original_path.clone();
                    }
                    diffs.push(entry);
                    additions += added;
                    deletions += removed;
                }
            }
            // Code errors the agent reported after an otherwise successful
            // edit, for the files it changed. Bounded like the runtime's.
            if let Some(reported) = object
                .remove("diagnostics")
                .as_ref()
                .and_then(serde_json::Value::as_array)
            {
                let diagnostics = reported
                    .iter()
                    .filter_map(|entry| {
                        let path = entry.get("path")?.as_str()?;
                        let message = entry.get("message")?.as_str()?;
                        let line = entry.get("line")?.as_u64().filter(|line| *line > 0)?;
                        let column = entry.get("column")?.as_u64().filter(|column| *column > 0)?;
                        let path = sanitize_path_value(
                            &sanitize_ansi_and_secrets(path, 16 * 1024),
                            workspace,
                        );
                        paths.contains(&path).then(|| {
                            serde_json::json!({"path": path, "line": line, "column": column,
                                "message": sanitize_ansi_and_secrets(message, 512)})
                        })
                    })
                    .take(10)
                    .collect::<Vec<_>>();
                if diagnostics.is_empty() {
                    object.remove("diagnosticsReporter");
                } else {
                    object.insert("diagnostics".into(), serde_json::json!(diagnostics));
                }
            } else {
                object.remove("diagnosticsReporter");
            }
            object.insert("paths".into(), serde_json::json!(paths));
            object.insert("additions".into(), serde_json::json!(additions));
            object.insert("deletions".into(), serde_json::json!(deletions));
            object.insert("diffs".into(), serde_json::json!(diffs));
            object.insert(
                "fileChangeVersion".into(),
                serde_json::json!(FILE_CHANGE_PROJECTION_VERSION),
            );
            object.remove("changes");
        }
        sanitize_json_value(&mut value, 16 * 1024, workspace, message);
    }
    value
}

fn bounded_computer_surface(result: &serde_json::Value) -> Option<serde_json::Value> {
    let surface = result.pointer("/_meta/codex~1toolSurface")?;
    if surface.get("kind")?.as_str()? != "computerUse" {
        return None;
    }
    let app = surface.get("app")?;
    if app.is_null() {
        return Some(serde_json::json!({"kind": "computerUse", "app": null}));
    }
    if app.get("kind")?.as_str()? != "appId" {
        return None;
    }
    let id = app.get("appId")?.as_str()?;
    if id.is_empty()
        || id.len() > 150
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    {
        return None;
    }
    Some(serde_json::json!({"kind": "computerUse", "app": {"kind": "appId", "appId": id}}))
}

pub(super) fn typed_thread_item_projection(
    entry: &AppServerThreadItem,
    fallback_time: &str,
    workspace: Option<&str>,
) -> ConversationThreadItem {
    typed_thread_item_projection_with_lifecycle(entry, fallback_time, workspace, None, 0)
}

/// The words a saved thread item shows in the history.
pub(crate) fn shown_text(turn: &str, item: serde_json::Value) -> Option<String> {
    typed_thread_item_projection(
        &AppServerThreadItem {
            turn_id: turn.to_owned(),
            item,
        },
        "1970-01-01T00:00:00Z",
        None,
    )
    .text
}

pub(super) fn typed_thread_item_projection_with_lifecycle(
    entry: &AppServerThreadItem,
    fallback_time: &str,
    workspace: Option<&str>,
    lifecycle_state: Option<&str>,
    lifecycle_authority: u8,
) -> ConversationThreadItem {
    let raw_type = entry
        .item
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let item_type = if known_thread_item_type(raw_type) {
        raw_type
    } else {
        "unknown"
    };
    let timestamp = typed_item_timestamp(&entry.item, fallback_time);
    let state = lifecycle_state
        .map(thread_item_state)
        .unwrap_or_else(|| item_state_from_value(&entry.item));
    let payload = sanitize_typed_item_in_state(&entry.item, workspace, state);
    let text = typed_item_display_text(&payload);
    let item_lifecycle_authority = if lifecycle_state.is_some() {
        lifecycle_authority
    } else if entry.item.get("status").is_some() || entry.item.get("state").is_some() {
        1
    } else {
        0
    };
    ConversationThreadItem {
        id: entry
            .item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown-item")
            .to_owned(),
        item_type: item_type.to_owned(),
        state: state.into(),
        text,
        payload: if item_type == "unknown" {
            serde_json::json!({"originalType": raw_type, "raw": payload})
        } else {
            payload
        },
        created_at: timestamp.clone(),
        updated_at: timestamp,
        source_rank: 3,
        lifecycle_authority: item_lifecycle_authority,
    }
}

#[cfg(test)]
pub(super) fn conversation_thread_projection(
    thread_id: Option<String>,
    messages: &[wonder_store::StoredMessage],
    assistant_messages: &[wonder_store::StoredAssistantMessage],
    events: &[HostEventEnvelope],
) -> ConversationThreadProjection {
    conversation_thread_projection_with_items(
        thread_id,
        messages,
        assistant_messages,
        events,
        &[],
        None,
    )
}

pub(super) fn conversation_thread_projection_with_items(
    thread_id: Option<String>,
    messages: &[wonder_store::StoredMessage],
    assistant_messages: &[wonder_store::StoredAssistantMessage],
    events: &[HostEventEnvelope],
    app_server_items: &[AppServerThreadItem],
    workspace: Option<&str>,
) -> ConversationThreadProjection {
    let mut turns: HashMap<String, ConversationTurn> = HashMap::new();
    let mut order = Vec::new();
    // A refresh can replace only some rows before a page is read. Choose the
    // newest native observation, rather than whichever old row appears first.
    // Actual Wonder turn receipts remain stronger than hydrated metadata.
    let observed_turns: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|message| message.codex_turn_id.as_deref())
        .chain(
            events
                .iter()
                .filter(|event| matches!(event.event, WonderEvent::MessageState { .. }))
                .filter_map(|event| event.turn_id.as_deref()),
        )
        .collect();
    let mut native_statuses: HashMap<String, (i64, u64, String, bool)> = HashMap::new();
    let mut native_timings: HashMap<String, NativeTurnTiming> = HashMap::new();
    for entry in app_server_items {
        let timestamp = typed_item_timestamp(&entry.item, "1970-01-01T00:00:00Z");
        ensure_conversation_turn(
            &mut turns,
            &mut order,
            &entry.turn_id,
            &timestamp,
            &timestamp,
        );
        if let Some(turn) = turns.get_mut(&entry.turn_id) {
            upsert_conversation_thread_item_at(
                turn,
                typed_thread_item_projection(entry, &timestamp, workspace),
                3,
            );
        }
    }
    for message in messages {
        let fallback_turn_id;
        let turn_id = if let Some(turn_id) = message.codex_turn_id.as_deref() {
            turn_id
        } else {
            fallback_turn_id = format!("local:{}", message.id);
            fallback_turn_id.as_str()
        };
        ensure_conversation_turn(
            &mut turns,
            &mut order,
            turn_id,
            &message.created_at,
            &message.created_at,
        );
        if let Some(turn) = turns.get_mut(turn_id) {
            let item_id = turn
                .items
                .iter()
                .find(|item| {
                    item.item_type == "userMessage"
                        && item
                            .payload
                            .get("clientId")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|client_id| client_id == message.client_message_id)
                })
                .map(|item| item.id.clone())
                .unwrap_or_else(|| message.id.clone());
            upsert_durable_thread_item(
                turn,
                ConversationThreadItem {
                    id: item_id.clone(),
                    item_type: "userMessage".into(),
                    state: thread_item_state(&message.state).into(),
                    text: Some(message.body.clone()),
                    payload: serde_json::json!({
                        "clientId": message.client_message_id,
                        "deliveryState": message.state,
                        "attachments": []
                    }),
                    created_at: message.created_at.clone(),
                    updated_at: message.created_at.clone(),
                    source_rank: 2,
                    lifecycle_authority: 2,
                },
            );
            if let Some(item) = turn.items.iter_mut().find(|item| item.id == item_id) {
                if let Some(payload) = item.payload.as_object_mut() {
                    payload.insert(
                        "clientId".into(),
                        serde_json::Value::String(message.client_message_id.clone()),
                    );
                    payload.insert(
                        "deliveryState".into(),
                        serde_json::Value::String(message.state.clone()),
                    );
                }
            }
        }
    }
    for message in assistant_messages {
        ensure_conversation_turn(
            &mut turns,
            &mut order,
            &message.codex_turn_id,
            &message.created_at,
            &message.updated_at,
        );
        if let Some(turn) = turns.get_mut(&message.codex_turn_id) {
            upsert_durable_thread_item(
                turn,
                ConversationThreadItem {
                    id: message.item_id.clone(),
                    item_type: "agentMessage".into(),
                    state: thread_item_state(&message.state).into(),
                    // Legacy deltas append to this exact source. The native
                    // row prepares its display without rewriting the stream.
                    text: Some(message.text.clone()),
                    payload: serde_json::json!({}),
                    created_at: message.created_at.clone(),
                    updated_at: message.updated_at.clone(),
                    source_rank: 2,
                    lifecycle_authority: 2,
                },
            );
        }
    }
    for event in events {
        let Some(turn_id) = event.turn_id.as_deref() else {
            continue;
        };
        let timestamp = event.occurred_at.clone();
        ensure_conversation_turn(&mut turns, &mut order, turn_id, &timestamp, &timestamp);
        if let Some(turn) = turns.get_mut(turn_id) {
            match &event.event {
                WonderEvent::MessageState { state } => {
                    if matches!(
                        state,
                        DeliveryState::AcceptedByCodex | DeliveryState::Streaming
                    ) && turn
                        .started_at
                        .as_ref()
                        .is_none_or(|start| timestamp < *start)
                    {
                        turn.started_at = Some(timestamp.clone());
                    }
                    if matches!(
                        state,
                        DeliveryState::Completed
                            | DeliveryState::Failed
                            | DeliveryState::Interrupted
                            | DeliveryState::Uncertain
                    ) {
                        turn.completed_at = Some(timestamp.clone());
                    }
                    turn.status = match state {
                        DeliveryState::Completed => "completed",
                        DeliveryState::Failed => "failed",
                        DeliveryState::Interrupted => "interrupted",
                        DeliveryState::Streaming
                        | DeliveryState::AcceptedByCodex
                        | DeliveryState::DispatchingToCodex => "inProgress",
                        _ => "unknown",
                    }
                    .into();
                }
                WonderEvent::ComputerUseScreenshot { image_url } => {
                    upsert_conversation_thread_item(
                        turn,
                        ConversationThreadItem {
                            id: event
                                .item_id
                                .clone()
                                .unwrap_or_else(|| event.event_id.clone()),
                            item_type: "imageView".into(),
                            state: "completed".into(),
                            text: Some("Computer screenshot".into()),
                            payload: serde_json::json!({ "imageUrl": image_url }),
                            created_at: timestamp.clone(),
                            updated_at: timestamp.clone(),
                            source_rank: 1,
                            lifecycle_authority: 1,
                        },
                    )
                }
                WonderEvent::ApprovalOpened { request_id } => upsert_conversation_thread_item(
                    turn,
                    ConversationThreadItem {
                        id: event.item_id.clone().unwrap_or_else(|| request_id.clone()),
                        item_type: "approval".into(),
                        state: "waiting".into(),
                        text: Some("Approval needed".into()),
                        payload: serde_json::json!({ "requestId": request_id }),
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                        source_rank: 1,
                        lifecycle_authority: 1,
                    },
                ),
                WonderEvent::ApprovalResolved {
                    request_id,
                    decision,
                } => upsert_conversation_thread_item(
                    turn,
                    ConversationThreadItem {
                        id: event.item_id.clone().unwrap_or_else(|| request_id.clone()),
                        item_type: "approval".into(),
                        state: "completed".into(),
                        text: Some("Approval resolved".into()),
                        payload: serde_json::json!({ "requestId": request_id, "decision": decision }),
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                        source_rank: 1,
                        lifecycle_authority: 1,
                    },
                ),
                WonderEvent::TerminalError { message } => upsert_conversation_thread_item(
                    turn,
                    ConversationThreadItem {
                        id: event
                            .item_id
                            .clone()
                            .unwrap_or_else(|| event.event_id.clone()),
                        item_type: "error".into(),
                        state: "failed".into(),
                        text: Some(message.clone()),
                        payload: serde_json::json!({}),
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                        source_rank: 1,
                        lifecycle_authority: 1,
                    },
                ),
                WonderEvent::Activity {
                    category,
                    state,
                    detail,
                } => {
                    if category == "thread_item_upsert" {
                        if let Some(detail) = detail.as_deref().and_then(|detail| {
                            serde_json::from_str::<serde_json::Value>(detail).ok()
                        }) {
                            if let (Some(turn_id), Some(item)) = (
                                detail.get("turnId").and_then(serde_json::Value::as_str),
                                detail.get("item"),
                            ) {
                                ensure_conversation_turn(
                                    &mut turns, &mut order, turn_id, &timestamp, &timestamp,
                                );
                                if let Some(target) = turns.get_mut(turn_id) {
                                    // Native project turns may have no Wonder
                                    // receipt; hydration carries the provider
                                    // outcome of turns run on the Mac.
                                    let elsewhere = detail
                                        .get("turnRunningElsewhere")
                                        .and_then(serde_json::Value::as_bool)
                                        == Some(true);
                                    if let Some(status) = detail
                                        .get("turnStatus")
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|s| {
                                            matches!(
                                                *s,
                                                "completed" | "failed" | "interrupted" | "unknown"
                                            ) || (elsewhere && *s == "inProgress")
                                        })
                                    {
                                        let observed = detail
                                            .get("turnStatusObservedAtMs")
                                            .and_then(serde_json::Value::as_i64)
                                            .unwrap_or(0);
                                        let candidate = (
                                            observed,
                                            event.sequence,
                                            status.to_owned(),
                                            elsewhere,
                                        );
                                        if native_statuses.get(turn_id).is_none_or(|previous| {
                                            (observed, event.sequence) > (previous.0, previous.1)
                                        }) {
                                            native_statuses.insert(turn_id.to_owned(), candidate);
                                        }
                                    }
                                    let clock = |key: &str| {
                                        detail
                                            .get(key)
                                            .and_then(serde_json::Value::as_i64)
                                            .filter(|at| *at >= 0)
                                    };
                                    let timing =
                                        native_timings.entry(turn_id.to_owned()).or_default();
                                    if let Some(started) = clock("turnStartedAtMs") {
                                        timing.started_ms = Some(started);
                                    }
                                    if let Some(completed) = clock("turnCompletedAtMs") {
                                        timing.completed_ms = Some(completed);
                                    }
                                    let entry = AppServerThreadItem {
                                        turn_id: turn_id.to_owned(),
                                        item: item.clone(),
                                    };
                                    let lifecycle_authority = detail
                                        .get("lifecycleAuthority")
                                        .and_then(serde_json::Value::as_u64)
                                        .map(|value| value.min(u8::MAX as u64) as u8)
                                        .unwrap_or_else(|| {
                                            // Pre-slice events did not carry an explicit authority.
                                            // Refresh events are status-less hydration; every other
                                            // stored upsert has an observed lifecycle state.
                                            if detail
                                                .get("historyRefresh")
                                                .and_then(serde_json::Value::as_bool)
                                                == Some(true)
                                            {
                                                0
                                            } else {
                                                1
                                            }
                                        });
                                    upsert_conversation_thread_item_at(
                                        target,
                                        typed_thread_item_projection_with_lifecycle(
                                            &entry,
                                            &timestamp,
                                            workspace,
                                            (lifecycle_authority > 0)
                                                .then(|| {
                                                    detail
                                                        .get("state")
                                                        .and_then(serde_json::Value::as_str)
                                                })
                                                .flatten(),
                                            lifecycle_authority,
                                        ),
                                        1,
                                    );
                                    if lifecycle_authority == 2
                                        && detail
                                            .get("historyRefresh")
                                            .and_then(serde_json::Value::as_bool)
                                            != Some(true)
                                    {
                                        if let Some(projected) =
                                            target.items.iter_mut().find(|projected| {
                                                projected.item_type == "reasoning"
                                                    && Some(projected.id.as_str())
                                                        == item
                                                            .get("id")
                                                            .and_then(serde_json::Value::as_str)
                                            })
                                        {
                                            projected.payload["thinkingTiming"] =
                                                observed_thinking_timing(
                                                    projected.payload.get("thinkingTiming"),
                                                    detail
                                                        .get("state")
                                                        .and_then(serde_json::Value::as_str)
                                                        .unwrap_or(""),
                                                    event,
                                                );
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    if matches!(
                        category.as_str(),
                        "thread/compacted" | "compaction" | "context compaction"
                    ) {
                        // Deprecated thread/compacted has no item identity. It
                        // may settle an already-known canonical marker, but it
                        // cannot fabricate one or merge separate compactions in
                        // the same turn.
                        let legacy_state = thread_item_state(state);
                        let compaction_count = turn
                            .items
                            .iter()
                            .filter(|item| item.item_type == "contextCompaction")
                            .count();
                        if compaction_count == 1 {
                            let item = turn
                                .items
                                .iter_mut()
                                .find(|item| item.item_type == "contextCompaction");
                            if let Some(item) = item {
                                if thread_item_state_rank(&item.state) < 4 {
                                    item.state = legacy_state.into();
                                    item.lifecycle_authority = item.lifecycle_authority.max(1);
                                    item.updated_at = timestamp.clone();
                                }
                            }
                        }
                        continue;
                    }
                    let (item_type, original_category) = thread_item_type_for_activity(category);
                    upsert_conversation_thread_item(
                        turn,
                        ConversationThreadItem {
                            id: event
                                .item_id
                                .clone()
                                .unwrap_or_else(|| event.event_id.clone()),
                            item_type: item_type.into(),
                            state: thread_item_state(state).into(),
                            text: detail.clone(),
                            payload: thread_activity_payload(
                                &original_category.unwrap_or_else(|| category.clone()),
                                detail.as_ref(),
                            ),
                            created_at: timestamp.clone(),
                            updated_at: timestamp.clone(),
                            source_rank: 1,
                            lifecycle_authority: 1,
                        },
                    );
                }
                WonderEvent::AssistantDelta { .. } | WonderEvent::AssistantCompleted { .. } => {}
                _ => upsert_conversation_thread_item(
                    turn,
                    ConversationThreadItem {
                        id: event
                            .item_id
                            .clone()
                            .unwrap_or_else(|| event.event_id.clone()),
                        item_type: "unknown".into(),
                        state: "unknown".into(),
                        text: None,
                        payload: serde_json::json!({ "event": event.event }),
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                        source_rank: 1,
                        lifecycle_authority: 1,
                    },
                ),
            }
        }
    }
    // Only an observed unfinished compaction inherits its turn's terminal
    // outcome. A status-less historical marker may have completed long before
    // its parent was interrupted; absence of lifecycle evidence stays unknown.
    for turn in turns.values_mut() {
        if !observed_turns.contains(turn.id.as_str()) {
            if let Some((_, _, status, elsewhere)) = native_statuses.get(&turn.id) {
                turn.status = status.clone();
                turn.running_elsewhere = *elsewhere;
            }
        }
        // The Mac sees a turn run elsewhere, or read back from history, only
        // when its events arrive together, so its own clock would say 0s. The
        // provider's turn clocks are what actually happened.
        if let Some(timing) = native_timings.get(&turn.id) {
            let format = |ms: i64| {
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            };
            if let Some(started) = timing.started_ms.and_then(format) {
                turn.started_at = Some(started);
            }
            if let Some(completed) = timing.completed_ms.and_then(format) {
                turn.completed_at = Some(completed);
            }
        }
        let Some(terminal_item_state) = (match turn.status.as_str() {
            "completed" => Some("completed"),
            "failed" => Some("failed"),
            "interrupted" => Some("interrupted"),
            _ => None,
        }) else {
            continue;
        };
        let turn_updated_at = turn.updated_at.clone();
        for item in turn
            .items
            .iter_mut()
            .filter(|item| item.item_type == "contextCompaction")
        {
            if matches!(item.state.as_str(), "started" | "inProgress") {
                item.state = terminal_item_state.into();
                item.lifecycle_authority = item.lifecycle_authority.max(3);
                item.updated_at = turn_updated_at.clone();
            }
        }
    }
    let mut result = order
        .into_iter()
        .filter_map(|id| turns.remove(&id))
        .collect::<Vec<_>>();
    // A turn's place is when it started: Cupola's record of starting it, or its
    // first item from the agent. A user message's time is when it was typed,
    // which for one that waited in the queue is before the reply it waited for.
    result.sort_by_cached_key(|turn| {
        turn.items
            .iter()
            .filter(|item| item.item_type != "userMessage")
            .map(|item| history_timestamp_key(&item.created_at))
            .chain(turn.started_at.as_deref().map(history_timestamp_key))
            .min()
            .unwrap_or_else(|| history_timestamp_key(&turn.created_at))
    });
    for turn in &mut result {
        if let Some(index) = turn
            .items
            .iter()
            .position(|item| item.item_type == "turnDiff")
        {
            let mut diff = turn.items.remove(index).payload;
            if let Some(object) = diff.as_object_mut() {
                object.retain(|key, _| {
                    matches!(key.as_str(), "paths" | "diffs" | "additions" | "deletions")
                });
            }
            turn.edited_files = Some(diff);
        }
        if turn
            .items
            .iter()
            .any(|item| item.payload["t3Position"].as_i64().is_some())
        {
            // T3 supplies timeline positions even when several imports share
            // a timestamp; compare one complete key, never mixed orderings.
            turn.items
                .sort_by_key(|item| item.payload["t3Position"].as_i64().unwrap_or(i64::MAX));
        } else {
            turn.items
                .sort_by_cached_key(|item| history_timestamp_key(&item.created_at));
        }
        // Individual commands can fail, finish, or retain stale running state
        // while the response continues. Only turn lifecycle events set status;
        // older history without those events must remain unknown.
    }
    ConversationThreadProjection {
        thread_id,
        turns: result,
        next_cursor: None,
        hydrated: false,
    }
}

pub(super) async fn hydrate_app_server_turn_items(
    state: &AppState,
    thread_id: &str,
    target_turn: &str,
) -> Option<Vec<AppServerThreadItem>> {
    let app_server = crate::claude::for_thread(state, thread_id).await.ok()?;
    let mut cursor = None;
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let mut params = serde_json::json!({
            "threadId": thread_id,
            "limit": 100,
            "sortDirection": "asc",
        });
        if let Some(cursor) = cursor.take() {
            params["cursor"] = serde_json::Value::String(cursor);
        }
        let response = app_server.request("thread/items/list", params).await.ok()?;
        if response.error.is_some() {
            return None;
        }
        let result = response.result?;
        {
            let data = result.get("data").and_then(serde_json::Value::as_array)?;
            for entry in data {
                let turn_id = entry.get("turnId").and_then(serde_json::Value::as_str)?;
                let item = entry.get("item").filter(|item| item.is_object())?;
                if turn_id == target_turn {
                    items.push(AppServerThreadItem {
                        turn_id: turn_id.to_owned(),
                        item: item.clone(),
                    });
                }
            }
        }
        cursor = match result.get("nextCursor") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(value))
                if !value.is_empty() && seen.insert(value.clone()) =>
            {
                Some(value.clone())
            }
            _ => return None,
        };
        if cursor.is_none() {
            return Some(items);
        }
    }
    None
}

/// Native clients ask for pages without data they never render: the raw event
/// list and inline computer-use screenshots (megabytes each). Older hosts
/// ignore the header and return the full page, which those clients also accept.
pub(super) const COMPACT_VIEW_HEADER: &str = "x-wonder-history-view";

pub(super) fn wants_compact_view(headers: &HeaderMap) -> bool {
    headers
        .get(COMPACT_VIEW_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("compact"))
}

fn is_inline_image(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|url| url.starts_with("data:"))
}

pub(super) fn compact_thread(thread: &mut ConversationThreadProjection) {
    for item in thread
        .turns
        .iter_mut()
        .flat_map(|turn| turn.items.iter_mut())
    {
        // The envelope is authoritative. Keep distinct provider values, but
        // don't send a second copy of the same text or item identity.
        if let Some(payload) = item.payload.as_object_mut() {
            if payload.get("id").and_then(serde_json::Value::as_str) == Some(&item.id) {
                payload.remove("id");
            }
            if item.text.as_deref().is_some_and(|text| {
                payload.get("text").and_then(serde_json::Value::as_str) == Some(text)
            }) {
                payload.remove("text");
            }
        }
        // Native command details render payload.output (or aggregatedOutput).
        // Keep legacy/nonduplicated text: only discard an exact redundant copy.
        if item.item_type == "commandExecution"
            && item.text.as_deref().is_some_and(|text| {
                item.payload
                    .get("output")
                    .or_else(|| item.payload.get("aggregatedOutput"))
                    .and_then(serde_json::Value::as_str)
                    == Some(text)
            })
        {
            item.text = None;
        }
        if item.item_type == "imageView"
            && item.payload.get("imageUrl").is_some_and(is_inline_image)
        {
            if let Some(payload) = item.payload.as_object_mut() {
                payload.remove("imageUrl");
            }
        }
    }
}

const COMPACT_OUTPUT_BYTES: usize = 2048;

/// Only stored history can promise a later output read. Outside-history and
/// agent-task transcripts retain their complete output until they have an
/// equivalent durable lookup. The ordinary (noncompact) projection is intact.
fn compact_history_outputs(thread: &mut ConversationThreadProjection) {
    for item in thread.turns.iter_mut().flat_map(|turn| &mut turn.items) {
        if !matches!(
            item.item_type.as_str(),
            "commandExecution" | "functionCallOutput"
        ) {
            continue;
        }
        let Some(payload) = item.payload.as_object_mut() else {
            continue;
        };
        let Some(output) = payload.get("output").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let bytes = output.len();
        let mut end = bytes.min(COMPACT_OUTPUT_BYTES);
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        let preview = output[..end].to_owned();
        // functionCallOutput also projects output into envelope text. It must
        // not bypass the preview through that redundant display field.
        if item
            .text
            .as_deref()
            .is_some_and(|text| text == output || text == sanitize_text(output, 16 * 1024, false))
        {
            item.text = None;
        }
        payload.insert("output".into(), preview.into());
        payload.insert("outputBytes".into(), bytes.into());
        payload.insert("outputTruncated".into(), (end < bytes).into());
    }
}

/// Keeps the event identity, sequence and type for replay while dropping the
/// inline screenshot a compact client never reads.
pub(super) fn compact_event(event: &mut HostEventEnvelope) {
    if let WonderEvent::ComputerUseScreenshot { image_url } = &mut event.event {
        image_url.clear();
    }
}

/// Only running sidebar rows need a recent action. Reuse the indexed history
/// page and its durable turn boundaries; never hydrate a runtime or read all
/// history just to decorate a list. Missing recent evidence stays absent.
pub(super) async fn current_activity(
    state: &AppState,
    conversation: &str,
) -> Option<serde_json::Value> {
    let page = state
        .store
        .conversation_history_page(&state.host_epoch, conversation, None, 16)
        .await
        .ok()??;
    let thread = conversation_thread_projection_with_items(
        page.codex_thread_id,
        &page.messages,
        &page.assistant_messages,
        &page.events,
        &[],
        Some(&page.workspace_path),
    );
    let turn = thread
        .turns
        .into_iter()
        .rev()
        .find(|turn| !turn.id.starts_with("local:"))?;
    if turn.status != "inProgress" {
        return None;
    }
    turn.items
        .into_iter()
        .rev()
        .find_map(compact_current_activity)
}

/// Current-step labels need provider-declared actions, never command output,
/// diffs, arguments or message text. Keep this wire value small and inert.
pub(super) fn compact_current_activity(
    mut item: ConversationThreadItem,
) -> Option<serde_json::Value> {
    if !matches!(item.state.as_str(), "started" | "streaming" | "waiting")
        || matches!(
            item.item_type.as_str(),
            "userMessage"
                | "approval"
                | "error"
                | "unknown"
                | "providerSwitch"
                | "taskNotification"
                | "subAgentActivity"
        )
    {
        return None;
    }
    if item.payload.get("success") == Some(&serde_json::Value::Bool(false))
        || item.payload.pointer("/result/isError") == Some(&serde_json::Value::Bool(true))
        || item
            .payload
            .get("error")
            .is_some_and(|error| !error.is_null() && error.as_str() != Some(""))
    {
        return None;
    }
    if let Some(payload) = item.payload.as_object_mut() {
        payload.retain(|key, _| {
            matches!(
                key.as_str(),
                "toolAction"
                    | "commandActions"
                    | "commandActionsTruncated"
                    | "paths"
                    | "pathsTruncated"
                    | "query"
                    | "action"
                    | "server"
                    | "tool"
                    | "appContext"
                    | "agentNickname"
                    | "agentRole"
            )
        });
    }
    if serde_json::to_vec(&item.payload).ok()?.len() > 8192 {
        item.payload = serde_json::json!({});
    }
    item.text = None;
    serde_json::to_value(item).ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolOutputQuery {
    turn_id: String,
}

/// Same sanitized projection as the history page, expanded only on request.
/// The enclosing router applies the ordinary paired-device authorization.
pub(super) async fn tool_output(
    State(state): State<AppState>,
    Path((conversation_id, item_id)): Path<(String, String)>,
    Query(query): Query<ToolOutputQuery>,
) -> Response {
    let (workspace, events) = match state
        .store
        .conversation_history_item(
            &state.host_epoch,
            &conversation_id,
            &query.turn_id,
            &item_id,
        )
        .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let thread =
        conversation_thread_projection_with_items(None, &[], &[], &events, &[], Some(&workspace));
    let output = thread
        .turns
        .iter()
        .filter(|turn| turn.id == query.turn_id)
        .flat_map(|turn| &turn.items)
        .find(|item| {
            item.id == item_id
                && matches!(
                    item.item_type.as_str(),
                    "commandExecution" | "functionCallOutput"
                )
        })
        .and_then(|item| item.payload["output"].as_str());
    let Some(output) = output else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "conversationId": conversation_id, "turnId": query.turn_id, "itemId": item_id,
            "output": output, "outputBytes": output.len(),
        })),
    )
        .into_response()
}

pub(super) async fn conversation_snapshot(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Query(query): Query<HistoryQuery>,
    headers: HeaderMap,
) -> Response {
    if projects::is_outside_history(&conversation_id) {
        return projects::outside_history_snapshot(
            &state,
            &conversation_id,
            query.before.as_deref(),
            query.limit,
            wants_compact_view(&headers),
        )
        .await;
    }
    let compact = wants_compact_view(&headers);
    let before = match decode_history_cursor(query.before.as_deref(), &conversation_id) {
        Ok(value) => value,
        Err(()) => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid history cursor. Open the conversation again.",
            )
                .into_response()
        }
    };
    if query.limit.is_some_and(|limit| limit == 0 || limit > 100) {
        return (
            StatusCode::BAD_REQUEST,
            "History page size must be between 1 and 100.",
        )
            .into_response();
    }
    let snapshot = match state
        .store
        .conversation_history_page(
            &state.host_epoch,
            &conversation_id,
            before.as_ref(),
            query.limit.unwrap_or(100),
        )
        .await
    {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) if wonder_store::is_rewind_pending(&error) => {
            return crate::rewind::recovering()
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "conversation lookup failed",
            )
                .into_response()
        }
    };
    let wonder_store::CommittedConversationSnapshot {
        last_sequence,
        history_revision,
        workspace_path,
        messages,
        assistant_messages,
        mut attachment_ids,
        codex_thread_id,
        events,
        next_cursor,
    } = snapshot;
    let handoffs = match state
        .store
        .conversation_update_handoffs(&conversation_id)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // The receipt tracks the currently running continuation; its original
    // user bubble stays at the original turn throughout repeated updates.
    let mut projection_messages = messages.clone();
    for message in &mut projection_messages {
        if let Some(first) = handoffs
            .iter()
            .find(|h| h.message_id.as_deref() == Some(&message.id) && h.resumed_turn_id.is_some())
        {
            message.codex_turn_id = Some(first.stopped_turn_id.clone());
        }
    }
    let mut thread = conversation_thread_projection_with_items(
        codex_thread_id.clone(),
        &projection_messages,
        &assistant_messages,
        &events,
        &[],
        Some(&workspace_path),
    );
    // Older projections lost workspace paths or native move destinations.
    // Repair from read-only runtime history once; local pages remain available.
    if thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .any(|item| {
            item.item_type == "fileChange"
                && item.payload["fileChangeVersion"] != FILE_CHANGE_PROJECTION_VERSION
        })
        && state
            .store
            .needs_file_change_repair(&conversation_id, now_ms() as i64)
            .await
            .unwrap_or(false)
    {
        let _ = refresh_history_request(
            State(state.clone()),
            Path(conversation_id.clone()),
            Query(HistoryRefreshQuery { repair: true }),
        )
        .await;
    }
    let answers = match state.store.question_answer_messages(&conversation_id).await {
        Ok(messages) => messages,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    for turn in &mut thread.turns {
        turn.items.retain(|item| {
            item.item_type != "userMessage"
                || (!handoffs.iter().any(|h| {
                    item.payload
                        .get("clientId")
                        .and_then(serde_json::Value::as_str)
                        == Some(h.resume_client_id.as_str())
                }) && !answers.iter().any(|m| {
                    item.id == m.id
                        || item
                            .payload
                            .get("clientId")
                            .and_then(serde_json::Value::as_str)
                            == Some(m.client_message_id.as_str())
                }))
        });
    }
    // An agent's record of a message Cupola sent through T3 Code carries
    // that message's client ID, so the phone shows the two as one bubble:
    // Cupola's, with its files and delivery state. Labelled, not removed:
    // the phone keeps a cached item that a newer read leaves out, and
    // labels it from `message_records`.
    let echoes = crate::t3_code::echoes(&state, &conversation_id).await;
    let message_records = crate::t3_code::message_records(&echoes);
    for turn in thread.turns.iter_mut().filter(|_| !echoes.is_empty()) {
        for item in &mut turn.items {
            let Some(client) = echoes.get(&(turn.id.clone(), item.id.clone())) else {
                continue;
            };
            if let (true, Some(payload)) = (
                item.item_type == "userMessage",
                item.payload.as_object_mut(),
            ) {
                payload.insert("clientId".into(), serde_json::json!(client));
            }
        }
    }
    let messages = messages
        .into_iter()
        .filter(|m| !answers.iter().any(|hidden| hidden.id == m.id))
        .collect::<Vec<_>>();
    thread.next_cursor = next_cursor.map(|cursor| encode_history_cursor(&conversation_id, &cursor));
    let mut sources = match state.store.message_sources(&conversation_id).await {
        Ok(sources) => sources,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // Automation prompts show as scheduled, not as the owner's own words.
    match state
        .store
        .automation_message_sources(&conversation_id)
        .await
    {
        Ok(automated) => sources.extend(automated),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let mut delivery_errors = match state.store.message_delivery_errors(&conversation_id).await {
        Ok(errors) => errors,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if !sources.is_empty() {
        let by_client: HashMap<&str, &wonder_store::MessageSource> = messages
            .iter()
            .filter_map(|m| Some((m.client_message_id.as_str(), sources.get(&m.id)?)))
            .collect();
        for item in thread
            .turns
            .iter_mut()
            .flat_map(|turn| turn.items.iter_mut())
            .filter(|item| item.item_type == "userMessage")
        {
            let source = item
                .payload
                .get("clientId")
                .and_then(serde_json::Value::as_str)
                .and_then(|client| by_client.get(client));
            if let (Some(source), Some(payload)) = (source, item.payload.as_object_mut()) {
                payload.insert("source".into(), serde_json::json!(source));
            }
        }
    }
    let mut events = events;
    if !shows_thinking(&state, &conversation_id).await {
        hide_thinking(&mut thread);
        events.iter_mut().for_each(hide_thinking_event);
    }
    let events = if compact {
        compact_thread(&mut thread);
        compact_history_outputs(&mut thread);
        Vec::new()
    } else {
        events
    };
    let response = Json(ConversationSnapshot {
        conversation_id,
        host_epoch: state.host_epoch.clone(),
        last_sequence,
        history_revision,
        codex_thread_id,
        messages: {
            let mut projected = Vec::with_capacity(messages.len());
            for message in messages {
                let message_attachment_ids = attachment_ids.remove(&message.id).unwrap_or_default();
                let source = sources.get(&message.id).cloned();
                let delivery_error = delivery_errors.remove(&message.id);
                projected.push(ConversationMessage {
                    original_body_sha256: state
                        .store
                        .original_body_hash(&message.id)
                        .await
                        .ok()
                        .flatten(),
                    message_id: message.id,
                    client_message_id: message.client_message_id,
                    body: message.body,
                    state: message.state,
                    created_at: message.created_at,
                    body_sha256: message.body_sha256,
                    codex_thread_id: message.codex_thread_id,
                    codex_turn_id: message.codex_turn_id,
                    delivery_error,
                    attachment_ids: message_attachment_ids,
                    source,
                });
            }
            projected
        },
        assistant_messages: assistant_messages
            .into_iter()
            .map(|message| ConversationAssistantMessage {
                message_id: message.id,
                codex_thread_id: message.codex_thread_id,
                codex_turn_id: message.codex_turn_id,
                item_id: message.item_id,
                text: message.text,
                state: message.state,
                created_at: message.created_at,
                updated_at: message.updated_at,
            })
            .collect(),
        thread,
        events,
        message_records,
    })
    .into_response();
    crate::history_compression::response(&headers, response).await
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryQuery {
    before: Option<String>,
    limit: Option<u32>,
}

fn encode_history_cursor(conversation: &str, cursor: &wonder_store::HistoryCursor) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&(
            2,
            conversation,
            cursor.sort_ms,
            cursor.sequence,
            cursor.history_revision,
        ))
        .unwrap(),
    )
}

fn decode_history_cursor(
    value: Option<&str>,
    conversation: &str,
) -> Result<Option<wonder_store::HistoryCursor>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() > 2048 {
        return Err(());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ())?;
    let (version, scope, sort_ms, sequence, history_revision): (u8, String, i64, i64, i64) =
        serde_json::from_slice(&bytes)
            .or_else(|_| {
                serde_json::from_slice::<(u8, String, i64, i64)>(&bytes).and_then(|(v, c, m, s)| {
                    if v == 1 {
                        Ok((2, c, m, s, 0))
                    } else {
                        Err(serde::de::Error::custom("Invalid cursor version"))
                    }
                })
            })
            .map_err(|_| ())?;
    if version != 2 || scope != conversation || sequence < 1 || history_revision < 0 {
        return Err(());
    }
    Ok(Some(wonder_store::HistoryCursor {
        sort_ms,
        sequence,
        history_revision,
    }))
}

static HISTORY_REFRESH_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Chats whose history reads wait before reading each page until a test
/// removes them; and the chats whose read reached that wait.
#[cfg(test)]
pub(crate) static PAGE_HOLD: std::sync::Mutex<
    Option<(
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    )>,
> = std::sync::Mutex::new(None);

pub(super) async fn history_refresh_status(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
) -> Response {
    match state.store.conversation_thread(&conversation).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "History is temporarily unavailable. Try again.",
            )
                .into_response()
        }
    }
    match state
        .store
        .history_refresh_status(&conversation, now_ms() as i64)
        .await
    {
        Ok(status) => Json(status).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "History status is temporarily unavailable. Try again.",
        )
            .into_response(),
    }
}

/// The newest native turn and whether an app on the Mac is running it. Cheap
/// enough to poll while a conversation is open; it reads no items.
pub(super) async fn history_activity(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
) -> Response {
    if !crate::projects::is_project(&state, &conversation).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let thread = match state.store.conversation_thread(&conversation).await {
        Ok(Some(thread)) => thread,
        // A new Project chat has no native session until its first message.
        Ok(None) => {
            return Json(serde_json::json!({"latestTurnId": null, "latestTurnStatus": "none", "runningElsewhere": false}))
                .into_response()
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if let Some(t3_thread) = crate::t3_code::feed::t3_thread(&thread) {
        return match crate::t3_code::feed::activity(&state, &conversation, t3_thread).await {
            Ok(activity) => Json(activity).into_response(),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    }
    // The phone is looking at this chat: wake it when another app writes the
    // session (see transcript_watch). Before the runtime is asked anything:
    // a runtime that is slow, restarting or failing for this thread answers
    // this check late or with an error, and the watch must still be renewed.
    if AgentFamily::issuing_thread(&thread) == AgentFamily::Claude {
        if let Ok(Some(stored)) = state.store.project_conversation(&conversation).await {
            if let Some(native) = stored.native_session_id.as_deref() {
                crate::transcript_watch::interest_claude(
                    &state,
                    &conversation,
                    native,
                    &stored.cwd,
                );
            }
        }
    }
    let Ok(runtime) = crate::claude::for_thread(&state, &thread).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(response) = runtime
        .request(
            "thread/turns/list",
            serde_json::json!({"threadId": thread, "limit": 1, "itemsView": "notLoaded"}),
        )
        .await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let latest = response
        .result
        .as_ref()
        .and_then(|result| result.get("data"))
        .and_then(serde_json::Value::as_array)
        .and_then(|turns| turns.first());
    let id = latest
        .and_then(|turn| turn.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let mut status = NativeTurnStatus {
        terminal_observed: latest
            .and_then(|turn| turn.get("completedAt"))
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|at| at >= 0),
        status: latest
            .and_then(|turn| turn.get("status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("none")
            .to_owned(),
        running_elsewhere: latest
            .and_then(|turn| turn.get("runningElsewhere"))
            .and_then(serde_json::Value::as_bool)
            == Some(true),
    };
    if let (Some(id), AgentFamily::Codex) = (&id, AgentFamily::issuing_thread(&thread)) {
        let mut statuses = HashMap::from([(id.clone(), status.clone())]);
        mark_codex_desktop_turn(&state.codex_home, &runtime, &thread, &mut statuses).await;
        if let Some(marked) = statuses.remove(id) {
            status = marked;
        }
    }
    // Process activity can continue after the last prompt completed. Reuse
    // native thread status without rewriting that prompt's outcome.
    let mut read_params = serde_json::json!({"threadId": thread, "includeTurns": false});
    if AgentFamily::issuing_thread(&thread) == AgentFamily::Claude {
        read_params["includeContextUsage"] = serde_json::Value::Bool(true);
    }
    let native_read = runtime
        .request("thread/read", read_params)
        .await
        .ok()
        .and_then(|response| response.result);
    if let (AgentFamily::Codex, Some(read)) = (AgentFamily::issuing_thread(&thread), &native_read) {
        crate::transcript_watch::interest_codex(&state, &conversation, &thread, read);
    }
    let thread_status = native_read.as_ref().and_then(|read| {
        (read
            .pointer("/thread/id")
            .and_then(serde_json::Value::as_str)
            == Some(thread.as_str()))
        .then(|| read.pointer("/thread/status").cloned())
        .flatten()
        .filter(|status| {
            matches!(
                status.get("type").and_then(serde_json::Value::as_str),
                Some("active" | "idle" | "notLoaded" | "systemError")
            )
        })
    });
    // A Claude session open in Claude Code in a terminal keeps Wonder's
    // messages waiting. The desktop app's (or another Agent SDK app's) chats
    // are taken over on send, the last sender winning: that stops a reply it
    // is writing (`sendStopsMacReply`) or background work still running in its
    // process (`tasksOnMac`), and the phone says so before sending.
    let stored = state.store.project_conversation(&conversation).await;
    // Where the thread's usage is written besides Wonder's own reports.
    let usage_source = match (&stored, AgentFamily::issuing_thread(&thread)) {
        (Ok(Some(stored)), AgentFamily::Claude) => stored
            .native_session_id
            .as_deref()
            .and_then(|native| crate::desktop_activity::claude_transcript(&stored.cwd, native))
            .map(crate::context_usage::Source::ClaudeTranscript),
        (_, AgentFamily::Codex) => native_read
            .as_ref()
            .and_then(|result| result.pointer("/thread/path"))
            .and_then(serde_json::Value::as_str)
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(crate::context_usage::Source::CodexRollout),
        _ => None,
    };
    // The provider's own history file changes whenever its history does, even
    // inside a finished turn (Claude adds its reply when a background task
    // ends), so an opening phone can tell whether its saved copy is current.
    let history_revision = match &usage_source {
        Some(source) => history_file_revision(source.path()).await,
        None => None,
    };
    let context_usage = match native_read
        .as_ref()
        .filter(|read| {
            AgentFamily::issuing_thread(&thread) == AgentFamily::Claude
                && read
                    .pointer("/thread/id")
                    .and_then(serde_json::Value::as_str)
                    == Some(thread.as_str())
        })
        .and_then(|read| read.get("contextUsage"))
    {
        Some(snapshot) => crate::context_usage::native_read(&thread, snapshot),
        None => crate::context_usage::resolve(&thread, usage_source).await,
    };
    let typical_window = context_usage.as_ref().and_then(|usage| {
        let model = stored
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .and_then(|c| c.model.as_deref());
        crate::context_usage::typical_window(usage, AgentFamily::issuing_thread(&thread), model)
    });
    // T3 Code: whether a message goes into its thread (see `t3_code`).
    let mut t3_code = None;
    let unheld = match &stored {
        Ok(Some(stored)) => Some(stored.clone()),
        _ => None,
    };
    // Who has the thread open on the Mac, read once for every check below.
    let mut seen = None;
    let (holder, tasks_on_mac) = match stored {
        Ok(Some(stored)) if AgentFamily::issuing_thread(&thread) == AgentFamily::Claude => {
            match stored.native_session_id.clone() {
                Some(native) => {
                    let app_sync = crate::agent_settings::claude_app_sync(&state).await;
                    let holder = crate::desktop_activity::claude_holder(&native)
                        .await
                        .map(|holder| holder.with_app_sync(app_sync));
                    seen = Some(holder.clone());
                    if holder.as_ref().is_some_and(crate::t3_code::holds) {
                        t3_code = Some(
                            crate::t3_code::delivery_seen(
                                &state,
                                &stored,
                                None,
                                crate::t3_code::Seen::Read(holder.as_ref()),
                            )
                            .await,
                        );
                    }
                    let tasks = match &holder {
                        Some(holder) if !status.running_elsewhere => {
                            tasks_running_on_mac(&runtime, &thread).await
                        }
                        _ => 0,
                    };
                    (holder, tasks)
                }
                None => (None, 0),
            }
        }
        // A Codex thread whose turn runs in a codex CLI or Codex's desktop
        // app: a message queues into that process until Send now.
        Ok(Some(stored))
            if AgentFamily::issuing_thread(&thread) == AgentFamily::Codex
                && status.running_elsewhere =>
        {
            let mut holder = match &stored.native_session_id {
                Some(native) => crate::desktop_activity::codex_holder(native).await,
                None => None,
            };
            // ChatGPT's desktop app takes a message into its running turn:
            // it does not wait (the phone says it goes through ChatGPT).
            if let (Some(holder), Some(native)) = (holder.as_mut(), &stored.native_session_id) {
                if holder.kind == crate::desktop_activity::HolderKind::Desktop
                    && crate::projects::relays_to_desktop(
                        &state,
                        stored.family,
                        native,
                        &stored.provider_store,
                    )
                    .await
                {
                    holder.waits = false;
                }
            }
            seen = Some(holder.clone());
            if holder.as_ref().is_some_and(crate::t3_code::holds) {
                t3_code = Some(
                    crate::t3_code::delivery_seen(
                        &state,
                        &stored,
                        None,
                        crate::t3_code::Seen::Read(holder.as_ref()),
                    )
                    .await,
                );
            }
            (
                holder.map(|holder| crate::desktop_activity::Holder {
                    replying: true,
                    ..holder
                }),
                0,
            )
        }
        _ => (None, 0),
    };
    // A message goes into T3 Code's thread there (`deliver_through_t3_code`):
    // it waits for nothing and stops nothing on the Mac. One T3 Code can't
    // carry waits for the thread as without T3 Code. The send path asks the
    // same question (`projects::send_waits`).
    let into_t3_code = t3_code
        .as_ref()
        .is_some_and(crate::t3_code::Delivery::sends);
    let holder = holder.map(|holder| crate::desktop_activity::Holder {
        waits: holder.waits && !into_t3_code,
        ..holder
    });
    let seen = match &seen {
        Some(holder) => crate::t3_code::Seen::Read(holder.as_ref()),
        None => crate::t3_code::Seen::Unread,
    };
    // A turn T3 Code runs: what it asks reaches the phone (`t3_code::questions`).
    if let (Some(holder), Some(stored)) = (&holder, &unheld) {
        if holder.replying && crate::t3_code::holds(holder) {
            if let Some(t3_thread) = crate::t3_code::route_seen(&state, stored, seen).await {
                crate::t3_code::questions::watch(&state, &stored.conversation_id, &t3_thread);
                crate::t3_code::stream::watch(&state, &stored.conversation_id, &t3_thread);
            }
        }
    }
    // A thread T3 Code owns that nothing on the Mac has open: Cupola runs
    // its reply only while T3 Code isn't running, and a message T3 Code's
    // send can't carry waits; the phone says which.
    let unheld_t3_code = match (&holder, &unheld) {
        (None, Some(stored)) => crate::t3_code::unheld_note(&state, stored, seen).await,
        _ => crate::t3_code::UnheldNote::default(),
    };
    // A message waits for this app (a terminal, background work); Send now
    // can take the chat over from it.
    let open_elsewhere = holder.as_ref().is_some_and(|holder| holder.waits);
    // Claude can add to a turn after it ended (its own reply when a background
    // task finishes); the item count lets an open screen notice that.
    let items = latest
        .and_then(|turn| turn.get("itemCount"))
        .and_then(serde_json::Value::as_u64);
    Json(serde_json::json!({
        "latestTurnId": id,
        "latestTurnStatus": status.status,
        "threadStatus": thread_status,
        "latestTurnItems": items,
        "historyRevision": history_revision,
        "runningElsewhere": status.running_elsewhere,
        "openElsewhere": open_elsewhere,
        "tasksOnMac": tasks_on_mac > 0 && !open_elsewhere && !into_t3_code,
        // The app on the Mac that has the chat open, for the phone's waiting
        // note and Send now's confirmation.
        "heldBy": holder.map(|holder| serde_json::json!({
            "app": holder.app,
            "kind": holder.kind,
            "replying": holder.replying,
            "backgroundTasks": tasks_on_mac,
            "background": holder.background || tasks_on_mac > 0,
            "waits": holder.waits,
            "canTakeOver": holder.can_take_over,
            "t3Code": t3_code.as_ref().map(crate::t3_code::Delivery::as_str),
            "t3CodeLimit": t3_code
                .as_ref()
                .and_then(crate::t3_code::Delivery::limit)
                .map(crate::t3_code::Limit::as_str),
            // Why a message doesn't go through T3 Code, for the phone's note.
            "t3CodeReason": t3_code.as_ref().and_then(crate::t3_code::Delivery::reason),
        })),
        "runsWithoutT3Code": unheld_t3_code.runs_without,
        // What T3 Code's send can't carry in the waiting message ("attachments",
        // "skills" or "model"), so it waits for the owner to change it.
        "t3CodeCantSend": unheld_t3_code.cant_send.map(crate::t3_code::Limit::as_str),
        "sendStopsMacReply": status.running_elsewhere
            && !open_elsewhere
            && !into_t3_code
            && AgentFamily::issuing_thread(&thread) == AgentFamily::Claude,
        // How full the context window is, only when the provider reported it;
        // the model's typical window fills in when it did not report one.
        "contextUsage": crate::context_usage::json_with_typical(context_usage.as_ref(), typical_window),
        // With no reading, why: the phone says so instead of showing nothing.
        "contextUsageNote": context_usage.is_none().then(|| crate::context_usage::note(AgentFamily::issuing_thread(&thread))),
    }))
    .into_response()
}

/// How many agent tasks, commands or monitors the desktop app's process for
/// this chat still runs. Its session record cannot tell (Claude Code 2.1.295
/// reports "idle" while a background command runs); the bridge reads them
/// from the transcript.
pub(crate) async fn tasks_running_on_mac(
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
) -> usize {
    task_count_on_mac(runtime, thread).await.unwrap_or(0)
}

/// `tasks_running_on_mac`, or None when the runtime could not answer.
pub(crate) async fn task_count_on_mac(
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
) -> Option<usize> {
    task_count(runtime, thread, true).await
}

/// All running tasks, including a process Cupola retains after its reply.
/// Lists cache this answer instead of asking for the runtime's loaded roster.
pub(crate) async fn task_count_for_list(
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
) -> Option<usize> {
    task_count(runtime, thread, false).await
}

async fn task_count(
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
    elsewhere_only: bool,
) -> Option<usize> {
    let response = runtime
        .request(
            "thread/backgroundTasks/list",
            serde_json::json!({"threadId": thread, "runningElsewhereOnly": elsewhere_only,
                "runningOnly": !elsewhere_only}),
        )
        .await
        .ok()?;
    let tasks = response.result.as_ref()?.get("data")?.as_array()?;
    Some(
        tasks
            .iter()
            .filter(|task| {
                let running = if elsewhere_only {
                    task.get("runningElsewhere")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                } else {
                    task.get("status").and_then(serde_json::Value::as_str) == Some("running")
                };
                // A linked session is its own chat; a foreground Bash run is the reply's.
                running
                    && !matches!(
                        task.get("kind").and_then(serde_json::Value::as_str),
                        Some("session" | "bash")
                    )
            })
            .count(),
    )
}

/// The receipt acknowledges a refresh job, never completed hydration. Readers
/// keep serving local pages; failures retain saved history and can be retried.
#[derive(Default, Deserialize)]
pub(super) struct HistoryRefreshQuery {
    /// Explicit repair bypasses source checkpoints; existing clients keep
    /// sending their unchanged empty POST body.
    #[serde(default)]
    repair: bool,
}

pub(super) async fn refresh_history(
    state: State<AppState>,
    conversation: Path<String>,
) -> Response {
    refresh_history_request(state, conversation, Query(Default::default())).await
}

pub(super) async fn refresh_history_request(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Query(query): Query<HistoryRefreshQuery>,
) -> Response {
    let thread = match state.store.conversation_thread(&conversation).await {
        Ok(Some(thread)) => thread,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                "No runtime history is available for this chat.",
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "History is temporarily unavailable. Try again.",
            )
                .into_response()
        }
    };
    let Ok(permit) = HISTORY_REFRESH_SLOTS.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Other history is refreshing. Try again shortly.",
        )
            .into_response();
    };
    let token = uuid::Uuid::new_v4().to_string();
    match state
        .store
        .claim_history_refresh(&conversation, &token, now_ms() as i64)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"state":"refreshing","detail":null})),
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "History refresh could not start. Try again.",
            )
                .into_response()
        }
    }
    let (finished, finished_status) = tokio::sync::oneshot::channel::<&'static str>();
    tokio::spawn(async move {
        let _permit = permit;
        // Sessions the thread left come first, so the current session's read
        // leaves its own checkpoint. One that cannot be read now stays owed;
        // it does not fail the refresh the owner asked for.
        if let Err(error) = import_left_history(&state, &conversation, &token).await {
            let _ = state.logger.record(
                "warn",
                "history_import_failed",
                serde_json::json!({"conversationId":conversation,"error":error}),
            );
        }
        let result =
            refresh_runtime_history(&state, &conversation, &thread, &token, query.repair).await;
        let (status, detail) = match result {
            Ok(()) => ("completed", None),
            Err(error) => {
                // Without the reason a blank thread cannot be diagnosed.
                let _ = state.logger.record(
                    "warn",
                    "history_refresh_failed",
                    serde_json::json!({"conversationId":conversation,"error":error}),
                );
                ("failed", Some("History refresh could not finish. Saved messages are still available. Try refreshing again."))
            }
        };
        if let Err(error) = state
            .store
            .update_history_refresh(&conversation, &token, status, now_ms() as i64, detail)
            .await
        {
            let _ = state.logger.record(
                "error",
                "history_refresh_status_failed",
                serde_json::json!({"error":error.to_string()}),
            );
            return;
        }
        let _ = finished.send(status);
    });
    // A refresh of a small chat finishes in tens of milliseconds. Answering with
    // its result saves the phone's status polls, each a round trip through the
    // relay; a longer one is polled as before. Still 202: it was accepted.
    match tokio::time::timeout(HISTORY_REFRESH_INLINE, finished_status).await {
        Ok(Ok("completed")) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"state":"completed","detail":null})),
        )
            .into_response(),
        _ => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"state":"refreshing","detail":null})),
        )
            .into_response(),
    }
}

/// Longest `POST .../history/refresh` waits for its refresh before it answers "refreshing".
const HISTORY_REFRESH_INLINE: std::time::Duration = std::time::Duration::from_millis(400);

/// Saves the provider history a thread is about to leave, so the switch
/// starts from a complete timeline. Returns whether it finished. When a busy
/// refresh slot, a slow runtime or a failure stops it, the switch leaves the
/// import owed on the left session and `import_left_history_later` (or the
/// thread's next refresh) finishes it.
pub(crate) async fn save_history_before_switch(state: &AppState, conversation: &str) -> bool {
    let Ok(Some(thread)) = state.store.conversation_thread(conversation).await else {
        return true;
    };
    if thread.is_empty() {
        return true;
    }
    let Ok(permit) = HISTORY_REFRESH_SLOTS.try_acquire() else {
        return false;
    };
    let token = uuid::Uuid::new_v4().to_string();
    if !matches!(
        state
            .store
            .claim_history_refresh(conversation, &token, now_ms() as i64)
            .await,
        Ok(true)
    ) {
        return false;
    }
    let finished = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        refresh_runtime_history(state, conversation, &thread, &token, false),
    )
    .await;
    drop(permit);
    let saved = matches!(finished, Ok(Ok(())));
    let (status, detail) = if saved {
        ("completed", None)
    } else {
        ("failed", Some("History refresh could not finish. Saved messages are still available. Try refreshing again."))
    };
    let _ = state
        .store
        .update_history_refresh(conversation, &token, status, now_ms() as i64, detail)
        .await;
    saved
}

/// Imports the history still owed by sessions the thread left, under the
/// caller's refresh claim. A session is marked done only after all of it was
/// read, so an interrupted import is retried and a repeated one saves
/// nothing twice (items keep their identity and first saved time).
async fn import_left_history(
    state: &AppState,
    conversation: &str,
    token: &str,
) -> Result<(), String> {
    let owed = state
        .store
        .history_imports_owed(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let mut failed = None;
    for entry in owed.into_iter().take(4) {
        let (Some(thread), Some(before)) = (
            entry.runtime_thread_id.as_deref(),
            entry.history_import_before_ms,
        ) else {
            continue;
        };
        // Read only sessions from this computer's current sign-in. An older
        // home cannot be resumed and does not hold up the other imports.
        let imported = async {
            let runtime =
                crate::providers::rpc_for_store(state, entry.family, &entry.provider_store).await?;
            read_runtime_history(
                state,
                conversation,
                thread,
                &runtime,
                token,
                false,
                Some(before),
            )
            .await?;
            state
                .store
                .finish_history_import(conversation, entry.id)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        if let Err(error) = imported {
            failed.get_or_insert(error);
        }
    }
    failed.map_or(Ok(()), Err)
}

/// A left session's item is placed by its own time, which the timeline
/// orders by. One without a readable time (or with one after the switch, which
/// it cannot have happened after) would otherwise be placed when it was
/// imported, after the switch row; it goes just before that row instead.
fn keep_before(item: &mut serde_json::Value, before_ms: i64) {
    let at = item
        .get("createdAt")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| match text.parse::<i64>() {
            Ok(ms) => Some(ms),
            Err(_) => chrono::DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|time| time.timestamp_millis()),
        });
    if at.is_some_and(|at| at < before_ms) {
        return;
    }
    if let (Some(object), Some(time)) = (
        item.as_object_mut(),
        chrono::DateTime::from_timestamp_millis(before_ms - 1),
    ) {
        object.insert(
            "createdAt".into(),
            time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                .into(),
        );
    }
}

/// Finishes a left session's import in the background after a switch whose
/// own save could not. Waits for a refresh slot instead of giving up; if
/// another refresh holds the thread, that refresh imports it.
pub(crate) fn import_left_history_later(state: &AppState, conversation: &str) {
    let (state, conversation) = (state.clone(), conversation.to_owned());
    tokio::spawn(async move {
        let wait = std::time::Duration::from_secs(120);
        let Ok(Ok(permit)) = tokio::time::timeout(wait, HISTORY_REFRESH_SLOTS.acquire()).await
        else {
            return;
        };
        let token = uuid::Uuid::new_v4().to_string();
        if !matches!(
            state
                .store
                .claim_history_refresh(&conversation, &token, now_ms() as i64)
                .await,
            Ok(true)
        ) {
            return;
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            import_left_history(&state, &conversation, &token),
        )
        .await
        .unwrap_or_else(|_| Err("History import timed out".into()));
        drop(permit);
        if let Err(error) = &result {
            // Still owed: the thread's next refresh tries again.
            let _ = state.logger.record(
                "warn",
                "history_import_failed",
                serde_json::json!({"conversationId":conversation,"error":error}),
            );
        }
        // The owner did not ask for this read, and the current session was
        // not read: nothing to report on the thread either way.
        let _ = state
            .store
            .update_history_refresh(&conversation, &token, "idle", now_ms() as i64, None)
            .await;
    });
}

async fn refresh_runtime_history(
    state: &AppState,
    conversation: &str,
    thread: &str,
    token: &str,
    repair: bool,
) -> Result<(), String> {
    // OpenCode and Cursor chats T3 Code runs are read from T3 Code's timeline.
    if let Some(t3) = crate::t3_code::feed::t3_thread(thread) {
        return crate::t3_code::feed::refresh(state, conversation, thread, t3, token).await;
    }
    // Read from this Project thread's owning provider store.
    let runtime = crate::claude::for_thread(state, thread).await?;
    read_runtime_history(state, conversation, thread, &runtime, token, repair, None).await
}

/// Saves one provider session's items into the thread's timeline. `left`
/// marks a session the thread switched away from, with the timeline position
/// its items stay before. Its settings are no longer the thread's, so they
/// are not synced.
async fn read_runtime_history(
    state: &AppState,
    conversation: &str,
    thread: &str,
    runtime: &wonder_app_server::RpcClient,
    token: &str,
    repair: bool,
    left: Option<i64>,
) -> Result<(), String> {
    let workspace = state
        .store
        .conversation_execution_directory(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let NativeHistoryState {
        statuses,
        timings,
        source,
        turns_revision,
        runtime_revision,
    } = if crate::projects::is_project(state, conversation).await {
        if left.is_none() {
            crate::native_settings::sync(state, conversation).await;
        }
        native_turn_statuses(&state.codex_home, runtime, thread).await?
    } else {
        NativeHistoryState::default()
    };
    let source_revision = match &source {
        Some(source) => history_file_revision(source).await,
        None => None,
    };
    // Vouched for by the provider's history file (the phone's same signal)
    // or by the runtime's own projection revision (the Claude bridge's items
    // also follow its task list and turn receipts, which change without a
    // transcript write). Include provider turn metadata and corrected
    // lifecycle too: a desktop process can finish or disappear without
    // another transcript write. An unchanged newest page alone is NOT
    // sufficient (Claude can append to an older completed turn).
    let checkpoint = turns_revision
        .as_ref()
        .filter(|_| source_revision.is_some() || runtime_revision.is_some())
        .map(|turns_revision| {
            let ordered: std::collections::BTreeMap<_, _> = statuses
                .iter()
                .map(|(id, status)| {
                    (
                        id,
                        (
                            &status.status,
                            status.terminal_observed,
                            status.running_elsewhere,
                        ),
                    )
                })
                .collect();
            hex::encode(Sha256::digest(
                serde_json::json!({
                    // The projection is the daemon's and the runtime's
                    // code: a new runtime process (a Cupola or provider
                    // update, any restart) re-reads once.
                    "projection":runtime.health().id(),
                    "source":source, "revision":source_revision, "turns":turns_revision,
                    "runtimeRevision":runtime_revision,
                    "statuses":ordered, "workspace":workspace,
                })
                .to_string()
                .as_bytes(),
            ))
        });
    {
        let _dispatch = state.dispatch_lock.lock().await;
        if !state
            .store
            .history_refresh_is_current(conversation, token)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Refresh superseded".into());
        }
        if !repair {
            if let Some(revision) = checkpoint.as_deref() {
                if state
                    .store
                    .history_source_is_current(conversation, thread, revision)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    drop(_dispatch);
                    crate::t3_code::reconcile_sends(state, conversation).await;
                    return Ok(());
                }
            }
        }
        if !state
            .store
            .set_history_source(conversation, token, thread, None)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Refresh superseded".into());
        }
    }
    let observed_at = now_ms() as i64;
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let mut retry_previews = false;
    loop {
        #[cfg(test)]
        {
            let held = || {
                PAGE_HOLD
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(held, _)| held.contains(conversation))
            };
            if held() {
                if let Some((_, reached)) = PAGE_HOLD.lock().unwrap().as_mut() {
                    reached.insert(conversation.to_owned());
                }
            }
            while held() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let response = runtime.request("thread/items/list", serde_json::json!({"threadId":thread,"limit":100,"sortDirection":"asc","cursor":cursor})).await.map_err(|e| e.to_string())?;
        if response.error.is_some() || !runtime.health().is_alive() {
            return Err("Runtime history is unavailable".into());
        }
        let result = response.result.ok_or("Missing history response")?;
        let entries = result
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or("Invalid history response")?;
        if entries.len() > 100 {
            return Err("Oversized history page".into());
        }
        let mut page = Vec::with_capacity(entries.len());
        for entry in entries {
            let turn = entry
                .get("turnId")
                .and_then(serde_json::Value::as_str)
                .ok_or("Missing history turn")?;
            let item = entry
                .get("item")
                .filter(|item| item.is_object())
                .ok_or("Missing history item")?;
            // Media trouble degrades one preview inside `normalize`; only
            // storage or runtime failures stop the refresh. A preview that
            // may be kept later leaves the history not current.
            let (mut item, retry) =
                tool_media::normalize_for_refresh(state, conversation, turn, item).await?;
            if let Some(before) = left {
                keep_before(&mut item, before);
            }
            retry_previews |= retry;
            let (item_state, authority) = history_item_lifecycle(entry, &item);
            let detail = if matches!(
                item["type"].as_str(),
                Some("contextCompaction" | "reasoning")
            ) {
                thread_item_upsert_detail_with_authority(
                    turn,
                    &item,
                    item_state,
                    authority,
                    workspace.as_deref(),
                )
            } else {
                thread_item_upsert_detail(turn, &item, item_state, workspace.as_deref())
            }
            .ok_or("Missing history item identity")?;
            let mut detail: serde_json::Value =
                serde_json::from_str(&detail).map_err(|e| e.to_string())?;
            detail["historyRefresh"] = serde_json::Value::Bool(true);
            if let Some(status) = statuses.get(turn) {
                detail["turnStatus"] = serde_json::Value::String(status.status.clone());
                detail["turnStatusObservedAtMs"] = observed_at.into();
                if status.running_elsewhere {
                    detail["turnRunningElsewhere"] = serde_json::Value::Bool(true);
                }
            }
            if let Some(timing) = timings.get(turn) {
                if let Some(started) = timing.started_ms {
                    detail["turnStartedAtMs"] = started.into();
                }
                if let Some(completed) = timing.completed_ms {
                    detail["turnCompletedAtMs"] = completed.into();
                }
            }
            let detail = detail.to_string();
            page.push(event_envelope(
                state,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(detail),
                },
                EventContext {
                    conversation_id: Some(conversation.into()),
                    thread_id: Some(thread.into()),
                    turn_id: Some(turn.into()),
                    item_id: item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    ..Default::default()
                },
            ));
        }
        // Normalize the native page outside the admission lock. Reset and a
        // provider switch change its token while holding this same lock, so a
        // stale page cannot save any item or broadcast any event after them.
        {
            let _dispatch = state.dispatch_lock.lock().await;
            if !state
                .store
                .history_refresh_is_current(conversation, token)
                .await
                .map_err(|e| e.to_string())?
            {
                return Err("Refresh superseded".into());
            }
            for envelope in state
                .store
                .commit_history_refresh(page)
                .await
                .map_err(|e| e.to_string())?
            {
                let _ = state.events.send(envelope);
            }
        }
        if !state
            .store
            .update_history_refresh(conversation, token, "refreshing", now_ms() as i64, None)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Refresh superseded".into());
        }
        cursor = match result.get("nextCursor") {
            None | Some(serde_json::Value::Null) => break,
            Some(serde_json::Value::String(value))
                if !value.is_empty() && value.len() <= 4096 && seen.insert(value.clone()) =>
            {
                Some(value.clone())
            }
            _ => return Err("Invalid history continuation".into()),
        };
        // Bound cursor bookkeeping too; this is an explicit retryable failure,
        // never a successful but silently truncated result.
        if seen.len() > 10_000 {
            return Err("History refresh page limit reached".into());
        }
        tokio::task::yield_now().await;
    }
    if let (Some(checkpoint), false) = (&checkpoint, retry_previews) {
        // Publish only what was read: neither signal moved during the read.
        let file_unchanged = match (&source, &source_revision) {
            (Some(source), Some(before)) => {
                history_file_revision(source).await.as_ref() == Some(before)
            }
            _ => true,
        };
        let runtime_unchanged = match &runtime_revision {
            Some(before) => {
                runtime_history_revision(runtime, thread).await.as_ref() == Some(before)
            }
            None => true,
        };
        if file_unchanged && runtime_unchanged {
            let _dispatch = state.dispatch_lock.lock().await;
            if !state
                .store
                .set_history_source(conversation, token, thread, Some(checkpoint))
                .await
                .map_err(|e| e.to_string())?
            {
                return Err("Refresh superseded".into());
            }
        }
    }
    crate::t3_code::reconcile_sends(state, conversation).await;
    Ok(())
}

/// The runtime's own revision of its projected items, when it has one.
async fn runtime_history_revision(
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
) -> Option<String> {
    let response = runtime
        .request(
            "thread/turns/list",
            serde_json::json!({"threadId": thread, "limit": 1, "itemsView": "notLoaded",
                "includeHistoryRevision": true}),
        )
        .await
        .ok()?;
    projection_revision(response.result.as_ref()?)
}

fn projection_revision(result: &serde_json::Value) -> Option<String> {
    result
        .get("historyRevision")
        .and_then(serde_json::Value::as_str)
        .filter(|revision| !revision.is_empty() && revision.len() <= 128)
        .map(str::to_owned)
}

/// Shared by the phone's activity response and the native hydration checkpoint.
async fn history_file_revision(path: &std::path::Path) -> Option<String> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let changed = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    // A same-size rewrite or replacement can keep its mtime; the inode and
    // status-change time still move.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            "{}:{changed}:{}:{}.{}",
            meta.len(),
            meta.ino(),
            meta.ctime(),
            meta.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    Some(format!("{}:{changed}", meta.len()))
}

/// The pinned ThreadItemEntry records producer clocks outside `item`.
/// A completed compaction or thinking item is positive evidence even when its
/// containing turn was later interrupted; the clocks are the only completion
/// evidence for these status-less items. Malformed or absent clocks do not
/// invent an outcome.
pub(super) fn history_item_lifecycle<'a>(
    entry: &serde_json::Value,
    item: &'a serde_json::Value,
) -> (&'a str, u8) {
    let state = item_state_from_value(item);
    let explicit = item.get("status").is_some() || item.get("state").is_some();
    if !matches!(
        item["type"].as_str(),
        Some("contextCompaction" | "reasoning")
    ) || matches!(state, "failed" | "interrupted")
    {
        return (state, u8::from(explicit));
    }
    if ["startedAtMs", "completedAtMs"].iter().any(|key| {
        entry.get(*key).is_some_and(|value| {
            !value.is_null() && !value.as_i64().is_some_and(|value| value >= 0)
        })
    }) {
        return (state, u8::from(explicit));
    }
    let start = entry
        .get("startedAtMs")
        .and_then(serde_json::Value::as_i64)
        .filter(|v| *v >= 0);
    let end = entry
        .get("completedAtMs")
        .and_then(serde_json::Value::as_i64)
        .filter(|v| *v >= 0);
    if end.is_some_and(|end| start.is_none_or(|start| end >= start)) {
        ("completed", 2)
    } else if start.is_some()
        && entry
            .get("completedAtMs")
            .is_none_or(serde_json::Value::is_null)
    {
        ("started", 2)
    } else {
        (state, u8::from(explicit))
    }
}

/// One `thread/items/list` entry read without the history ledger. A
/// status-less compaction or thinking item takes the lifecycle its producer
/// clocks prove, so thinking still running is never read as finished.
pub(super) fn app_server_entry_item(
    turn_id: &str,
    entry: &serde_json::Value,
    item: &serde_json::Value,
) -> AppServerThreadItem {
    let (state, authority) = history_item_lifecycle(entry, item);
    let mut item = item.clone();
    if authority == 2 {
        if let Some(object) = item.as_object_mut() {
            object.insert("status".into(), state.into());
        }
    }
    AppServerThreadItem {
        turn_id: turn_id.to_owned(),
        item,
    }
}

/// Outcome of a native turn. `running_elsewhere` marks a turn still running in
/// a desktop or terminal app on the Mac, which Wonder cannot steer or stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeTurnStatus {
    pub(crate) status: String,
    pub(crate) terminal_observed: bool,
    pub(crate) running_elsewhere: bool,
}

/// When the provider says a turn started and finished, in Unix milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NativeTurnTiming {
    pub(super) started_ms: Option<i64>,
    pub(super) completed_ms: Option<i64>,
}

/// Codex turns carry `startedAt`/`completedAt` in whole seconds and an exact
/// `durationMs`. The start plus the duration keeps a short turn's length
/// instead of rounding it to 0s or 1s. Malformed or reversed clocks are dropped.
pub(super) fn native_turn_timing(turn: &serde_json::Value) -> NativeTurnTiming {
    let clock = |key: &str| {
        turn.get(key)
            .and_then(serde_json::Value::as_i64)
            .filter(|value| *value >= 0)
    };
    let started_ms = clock("startedAt").and_then(|at| at.checked_mul(1000));
    let completed_ms = clock("completedAt").and_then(|at| at.checked_mul(1000));
    let completed_ms = match (started_ms, clock("durationMs"), completed_ms) {
        (Some(start), Some(duration), Some(_)) => start.checked_add(duration),
        (_, _, completed) => completed,
    }
    .filter(|end| started_ms.is_none_or(|start| *end >= start));
    NativeTurnTiming {
        started_ms,
        completed_ms,
    }
}

/// Terminal outcomes and provider clocks of native turns, including turns run
/// on the Mac.
#[derive(Default)]
struct NativeHistoryState {
    statuses: HashMap<String, NativeTurnStatus>,
    timings: HashMap<String, NativeTurnTiming>,
    source: Option<std::path::PathBuf>,
    /// None when the turn list could not be read completely: the refresh
    /// still runs, but it never vouches for an unchanged history.
    turns_revision: Option<String>,
    /// The runtime's revision of its projected items (the Claude bridge
    /// reports one; Codex does not).
    runtime_revision: Option<String>,
}

async fn native_turn_statuses(
    home: &std::path::Path,
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
) -> Result<NativeHistoryState, String> {
    let mut statuses = HashMap::new();
    let mut timings = HashMap::new();
    let mut revision = Sha256::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    let mut complete = true;
    let mut runtime_revision = None;
    for _ in 0..100 {
        let response = runtime
            .request(
                "thread/turns/list",
                serde_json::json!({"threadId": thread, "limit": 100, "itemsView": "notLoaded",
                    "cursor": cursor, "includeHistoryRevision": true}),
            )
            .await
            .map_err(|e| e.to_string())?;
        if response.error.is_some() || !runtime.health().is_alive() {
            return Err("Runtime turn history is unavailable".into());
        }
        let result = response.result.ok_or("Missing turn history")?;
        if seen.is_empty() {
            runtime_revision = projection_revision(&result);
        }
        // Lifecycle is best effort, as it was before the checkpoint: a page
        // without data or over its bound costs only the checkpoint.
        let turns = match result.get("data").and_then(serde_json::Value::as_array) {
            Some(turns) if turns.len() <= 100 => turns.as_slice(),
            Some(turns) => {
                complete = false;
                &turns[..100]
            }
            None => {
                complete = false;
                &[]
            }
        };
        for turn in turns {
            revision.update(turn.to_string().as_bytes());
            if let (Some(id), Some(status)) = (
                turn.get("id").and_then(serde_json::Value::as_str),
                turn.get("status").and_then(serde_json::Value::as_str),
            ) {
                let timing = native_turn_timing(turn);
                if timing != NativeTurnTiming::default() {
                    timings.insert(id.to_owned(), timing);
                }
                // The Claude bridge reads Claude Code's own busy record.
                let running_elsewhere = status == "inProgress"
                    && turn
                        .get("runningElsewhere")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                statuses.insert(
                    id.to_owned(),
                    NativeTurnStatus {
                        terminal_observed: turn
                            .get("completedAt")
                            .and_then(serde_json::Value::as_i64)
                            .is_some_and(|at| at >= 0),
                        status: status.to_owned(),
                        running_elsewhere,
                    },
                );
            }
        }
        cursor = match result.get("nextCursor") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(value))
                if !value.is_empty() && value.len() <= 4096 && seen.insert(value.clone()) =>
            {
                Some(value.clone())
            }
            _ => {
                complete = false;
                None
            }
        };
        if cursor.is_none() {
            break;
        }
    }
    if cursor.is_some() {
        complete = false;
    }
    let source = if AgentFamily::issuing_thread(thread) == AgentFamily::Codex {
        mark_codex_desktop_turn(home, runtime, thread, &mut statuses).await
    } else {
        None
    };
    Ok(NativeHistoryState {
        statuses,
        timings,
        source,
        turns_revision: complete.then(|| hex::encode(revision.finalize())),
        runtime_revision,
    })
}

/// Wonder's App Server reinterprets an unloaded foreign turn as interrupted.
/// Only the producer's indexed lifecycle can confirm that terminal outcome.
async fn mark_codex_desktop_turn(
    home: &std::path::Path,
    runtime: &wonder_app_server::RpcClient,
    thread: &str,
    statuses: &mut HashMap<String, NativeTurnStatus>,
) -> Option<std::path::PathBuf> {
    let Ok(response) = runtime
        .request("thread/read", serde_json::json!({"threadId": thread}))
        .await
    else {
        clear_unconfirmed_codex_interruption(statuses);
        return None;
    };
    let Some(read) = response.result else {
        clear_unconfirmed_codex_interruption(statuses);
        return None;
    };
    let thread_value = read.get("thread").unwrap_or(&read);
    reconcile_codex_turn_statuses(home, thread_value, statuses).await;
    (thread_value.get("id").and_then(serde_json::Value::as_str) == Some(thread))
        .then(|| {
            thread_value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
        })
        .flatten()
}

/// Reuse already verified native metadata when a caller has listed the thread.
/// This is the same cross-process correction used by conversation history.
pub(crate) async fn reconcile_codex_turn_statuses(
    home: &std::path::Path,
    thread_value: &serde_json::Value,
    statuses: &mut HashMap<String, NativeTurnStatus>,
) {
    let Some(thread) = thread_value.get("id").and_then(serde_json::Value::as_str) else {
        clear_unconfirmed_codex_interruption(statuses);
        return;
    };
    if let Some(path) = thread_value.get("path").and_then(serde_json::Value::as_str) {
        crate::desktop_activity::remember_codex_rollout(thread, path);
    }
    if crate::desktop_activity::codex_loaded_busy(thread_value) == Some(true) {
        return; // Positive active state belongs to this native runtime.
    }
    clear_unconfirmed_codex_interruption(statuses);
    let activity = crate::desktop_activity::codex_thread_activity(home, thread).await;
    if let Some(activity) = activity {
        if let Some(status) = statuses.get_mut(&activity.turn) {
            status.status = activity.status.into();
            status.running_elsewhere = activity.working();
        }
    }
}

fn clear_unconfirmed_codex_interruption(statuses: &mut HashMap<String, NativeTurnStatus>) {
    for status in statuses.values_mut() {
        if status.status == "interrupted" && !status.terminal_observed {
            status.status = "unknown".into();
            status.running_elsewhere = false;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "history_tests.rs"]
mod tests;

#[cfg(test)]
mod commentary_tests {
    // Unix fixtures (absolute /paths, /bin/sh or python3 fakes, sips); Windows paths are not covered yet.
    #[cfg(unix)]
    #[test]
    fn compact_history_size_and_utf8_output_contract() {
        use super::*;
        // Reproduce the measured page's byte budget, not its private contents:
        // 100 items, 234,000 JSON bytes, 55% string tool output, ~16.5 KB
        // duplicated message text and ~4 KB duplicated item identity.
        fn page_bytes(thread: &ConversationThreadProjection) -> usize {
            serde_json::to_vec(&serde_json::json!({
                "conversationId":"fixture", "hostEpoch":"epoch", "lastSequence":100,
                "historyRevision":0, "codexThreadId":"thread", "messages":[],
                "assistantMessages":[], "thread":thread, "events":[]
            }))
            .unwrap()
            .len()
        }
        let entries: Vec<_> = (0..100)
            .map(|index| AppServerThreadItem {
                turn_id: "turn".into(),
                item: if index < 26 {
                    serde_json::json!({"id": format!("item-{index:035}"), "type":"commandExecution",
                    "status":"completed", "command":"printf fixture", "output":"o".repeat(4950)})
                } else {
                    serde_json::json!({"id": format!("item-{index:035}"), "type":"agentMessage",
                    "status":"completed", "text":"m".repeat(223)})
                },
            })
            .collect();
        let mut page = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[],
            &entries,
            None,
        );
        // Pre-change compact view already removed command envelope text.
        for item in page.turns.iter_mut().flat_map(|turn| &mut turn.items) {
            if item.item_type == "commandExecution" {
                item.text = None;
            }
        }
        for item in &mut page.turns[0].items[..26] {
            item.payload["arguments"] = "".into();
        }
        let size = page_bytes(&page);
        let padding = 234_000 - size;
        for (index, item) in page.turns[0].items[..26].iter_mut().enumerate() {
            let bytes = padding / 26 + usize::from(index < padding % 26);
            assert!(
                bytes < 16 * 1024,
                "fixture input must obey the real sanitizer limit"
            );
            item.payload["arguments"] = "a".repeat(bytes).into();
        }
        let before = page_bytes(&page);
        assert_eq!(before, 234_000);
        let output_bytes: usize = page
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter_map(|item| item.payload["output"].as_str())
            .map(str::len)
            .sum();
        assert_eq!(output_bytes, 128_700);
        compact_thread(&mut page);
        compact_history_outputs(&mut page);
        let after = page_bytes(&page);
        println!("PERF-3 synthetic compact page: {before} -> {after} bytes; tool output {output_bytes} bytes (55%)");
        assert!(after <= 150_000, "compact page is {after} bytes");
        for item in page.turns.iter().flat_map(|turn| &turn.items) {
            assert!(item.payload.get("id").is_none());
            assert!(item.payload.get("text").is_none());
            if item.item_type == "commandExecution" {
                assert_eq!(item.payload["output"].as_str().unwrap().len(), 2048);
                assert_eq!(item.payload["outputBytes"], 4950);
                assert_eq!(item.payload["outputTruncated"], true);
                crate::tests::validate_http_contract("compactToolOutput", &item.payload);
            } else {
                assert_eq!(item.text.as_deref(), Some("m".repeat(223).as_str()));
            }
        }

        for output in [
            "".to_owned(),
            "x".repeat(2048),
            format!("{}🌞tail", "x".repeat(2047)),
        ] {
            let entry = AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({
                    "id":"tool", "type":"functionCallOutput", "output": output, "text":"distinct summary"
                }),
            };
            let mut page =
                conversation_thread_projection_with_items(None, &[], &[], &[], &[entry], None);
            compact_thread(&mut page);
            compact_history_outputs(&mut page);
            let item = &page.turns[0].items[0];
            let preview = item.payload["output"].as_str().unwrap();
            assert!(preview.len() <= 2048);
            assert!(output.starts_with(preview));
            assert_eq!(item.payload["outputBytes"], output.len());
            assert_eq!(item.payload["outputTruncated"], output.len() > 2048);
            assert_eq!(item.text.as_deref(), Some("distinct summary"));
        }
    }

    #[test]
    fn file_diff_survives_sanitized_projection() {
        let value = super::sanitize_typed_item(
            &serde_json::json!({"type":"fileChange", "changes":[{"path":"/tmp/work/hello.txt", "diff":"@@ -1 +1 @@\n-old\n+new"}]}),
            Some("/tmp/work"),
        );
        assert_eq!(value["diffs"][0]["diff"], "@@ -1 +1 @@\n-old\n+new");
        assert_eq!(value["additions"], 1);
        assert_eq!(value["deletions"], 1);
        assert!(value.get("changes").is_none());
        assert_eq!(super::sanitize_typed_item(&value, Some("/tmp/work")), value);

        // A native move keeps the old name for the diff and opens the new file.
        for (destination, expected) in [
            ("/tmp/work/moved.txt", "moved.txt"),
            ("nested/moved.txt", "nested/moved.txt"),
            ("/tmp/work-other/private.txt", "[path outside workspace]"),
        ] {
            let moved = super::sanitize_typed_item(
                &serde_json::json!({"type":"fileChange", "changes":[{
                    "path":"/tmp/work/hello.txt", "kind":{"type":"update", "move_path":destination},
                    "diff":"@@ -1 +1 @@\n-old\n+new"
                }]}),
                Some("/tmp/work"),
            );
            assert_eq!(moved["paths"], serde_json::json!([expected]));
            assert_eq!(moved["diffs"][0]["path"], expected);
            assert_eq!(moved["diffs"][0]["originalPath"], "hello.txt");
            assert_eq!(moved["diffs"][0]["kind"], "update");
            assert_eq!(moved["additions"], 1);
            assert_eq!(moved["deletions"], 1);
            assert_eq!(super::sanitize_typed_item(&moved, Some("/tmp/work")), moved);
        }
    }

    use super::*;

    #[test]
    fn thinking_time_requires_matched_observed_boundaries_and_survives_hydration() {
        fn event(
            sequence: u64,
            at: &str,
            state: &str,
            refresh: bool,
            epoch: &str,
        ) -> HostEventEnvelope {
            HostEventEnvelope {
                event_id: format!("event-{sequence}"), host_epoch: epoch.into(), sequence, occurred_at: at.into(),
                request_id: None, device_id: None, conversation_id: Some("conversation".into()), message_id: None,
                thread_id: Some("thread".into()), turn_id: Some("turn".into()), item_id: Some("thinking".into()), approval_id: None,
                event: WonderEvent::Activity { category: "thread_item_upsert".into(), state: "updated".into(),
                    detail: Some(serde_json::json!({"turnId":"turn", "itemId":"thinking", "state":state,
                        "lifecycleAuthority":if refresh {0} else {2}, "historyRefresh":refresh,
                        "item":{"id":"thinking", "type":"reasoning", "summary":["private thinking"],
                            "thinkingTiming":{"startedAtMs":1, "completedAtMs":999999, "finished":true}}}).to_string()) }
            }
        }
        let start = event(1, "1000", "started", false, "epoch");
        let duplicate = event(2, "9000", "started", false, "epoch");
        let end = event(3, "13900", "completed", false, "epoch");
        let refresh = event(4, "999999", "completed", true, "epoch");
        let active =
            conversation_thread_projection(None, &[], &[], &[start.clone(), refresh.clone()]);
        assert_eq!(active.turns[0].items[0].state, "started");
        assert_eq!(
            active.turns[0].items[0].payload["thinkingTiming"]["startedAtMs"],
            1000
        );
        let mut projected = conversation_thread_projection(
            None,
            &[],
            &[],
            &[start.clone(), duplicate, end.clone(), refresh.clone()],
        );
        hide_thinking(&mut projected);
        let item = &projected.turns[0].items[0];
        assert_eq!(item.payload["thinkingTiming"]["startedAtMs"], 1000);
        assert_eq!(item.payload["thinkingTiming"]["completedAtMs"], 13900);
        assert!(!serde_json::to_string(&projected)
            .unwrap()
            .contains("private thinking"));
        for events in [
            vec![refresh],
            vec![end.clone()],
            vec![end.clone(), start.clone()],
            vec![start.clone(), event(2, "900", "completed", false, "epoch")],
            vec![
                start.clone(),
                event(2, "13900", "completed", false, "other-epoch"),
            ],
            vec![
                start.clone(),
                event(2, "13900", "interrupted", false, "epoch"),
            ],
            vec![start.clone(), event(2, "13900", "failed", false, "epoch")],
            vec![start, event(2, "86401001", "completed", false, "epoch")],
        ] {
            let projected = conversation_thread_projection(None, &[], &[], &events);
            assert!(projected.turns[0].items[0]
                .payload
                .pointer("/thinkingTiming/completedAtMs")
                .is_none());
        }
    }

    #[test]
    fn native_timestamp_precision_orders_items_and_keeps_the_latest_payload() {
        let early = "2026-10-27T00:00:00.5Z";
        let later = "2026-10-27T00:00:00.500000001Z";
        let same = "2026-10-27T00:00:00.500000001+00:00";
        let mut turns = HashMap::new();
        let mut order = Vec::new();
        ensure_conversation_turn(&mut turns, &mut order, "turn", later, later);
        ensure_conversation_turn(&mut turns, &mut order, "turn", early, early);
        let turn = turns.get_mut("turn").unwrap();
        assert_eq!(turn.created_at, early);
        assert_eq!(turn.updated_at, later);
        let entry = AppServerThreadItem {
            turn_id: "turn".into(),
            item: serde_json::json!({"id":"item", "type":"commandExecution", "command":"latest"}),
        };
        upsert_conversation_thread_item(turn, typed_thread_item_projection(&entry, later, None));
        let mut stale = typed_thread_item_projection(&entry, early, None);
        stale.payload["command"] = "stale".into();
        upsert_conversation_thread_item(turn, stale);
        assert_eq!(turn.items[0].payload["command"], "latest");
        assert_eq!(turn.items[0].created_at, early);
        assert_eq!(turn.items[0].updated_at, later);

        let events = [("first", early), ("second", later), ("equal", same)]
            .into_iter()
            .enumerate()
            .map(|(index, (id, timestamp))| HostEventEnvelope {
                event_id: id.into(),
                host_epoch: "epoch".into(),
                sequence: index as u64 + 1,
                occurred_at: timestamp.into(),
                request_id: None,
                device_id: None,
                conversation_id: Some("chat".into()),
                message_id: None,
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: Some(id.into()),
                approval_id: None,
                event: WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(
                        serde_json::json!({"turnId":"turn", "itemId":id, "state":"completed",
                        "item":{"id":id, "type":"commandExecution"}})
                        .to_string(),
                    ),
                },
            })
            .collect::<Vec<_>>();
        let projection = conversation_thread_projection(None, &[], &[], &events);
        assert_eq!(
            projection.turns[0]
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "equal"]
        );
    }

    #[test]
    fn durable_text_and_lifecycle_phase_reconcile_in_either_order() {
        let entry = AppServerThreadItem {
            turn_id: "turn".into(),
            item: serde_json::json!({"id":"comment", "type":"agentMessage", "text":"Checking", "phase":"commentary"}),
        };
        let lifecycle = || typed_thread_item_projection(&entry, "1000", None);
        let durable = || {
            let mut item = lifecycle();
            item.payload = serde_json::json!({});
            item.text = Some("Checking the files".into());
            item.created_at = "2000".into();
            item
        };
        for lifecycle_first in [true, false] {
            let mut turn = ConversationTurn {
                started_at: None,
                completed_at: None,
                id: "turn".into(),
                status: "unknown".into(),
                created_at: "1000".into(),
                updated_at: "1000".into(),
                items: vec![],
                running_elsewhere: false,
                edited_files: None,
            };
            if lifecycle_first {
                upsert_conversation_thread_item_at(&mut turn, lifecycle(), 1);
                upsert_conversation_thread_item_at(&mut turn, durable(), 2);
            } else {
                upsert_conversation_thread_item_at(&mut turn, durable(), 2);
                upsert_conversation_thread_item_at(&mut turn, lifecycle(), 1);
            }
            assert_eq!(turn.items.len(), 1);
            assert_eq!(turn.items[0].payload["phase"], "commentary");
            assert_eq!(turn.items[0].created_at, "2000");
            assert_eq!(turn.items[0].text.as_deref(), Some("Checking the files"));
        }
    }

    #[test]
    fn reconnect_hydration_can_complete_an_ordinary_started_item() {
        let entry = AppServerThreadItem {
            turn_id: "turn".into(),
            item: serde_json::json!({"id":"command-1", "type":"commandExecution"}),
        };
        let started =
            typed_thread_item_projection_with_lifecycle(&entry, "1000", None, Some("started"), 2);
        let hydrated = typed_thread_item_projection(&entry, "2000", None);
        assert_eq!(hydrated.state, "completed");

        let mut turn = ConversationTurn {
            started_at: None,
            completed_at: None,
            id: "turn".into(),
            status: "inProgress".into(),
            created_at: "1000".into(),
            updated_at: "2000".into(),
            items: vec![],
            running_elsewhere: false,
            edited_files: None,
        };
        upsert_conversation_thread_item_at(&mut turn, started, 1);
        upsert_conversation_thread_item_at(&mut turn, hydrated, 3);
        assert_eq!(turn.items.len(), 1);
        assert_eq!(turn.items[0].state, "completed");
    }

    #[test]
    fn statusless_compaction_hydration_cannot_mask_explicit_lifecycle() {
        let entry = AppServerThreadItem {
            turn_id: "turn".into(),
            item: serde_json::json!({"id":"compact-1", "type":"contextCompaction"}),
        };
        let running =
            typed_thread_item_projection_with_lifecycle(&entry, "1000", None, Some("started"), 2);
        let hydrated = typed_thread_item_projection(&entry, "2000", None);
        assert_eq!(hydrated.state, "unknown");

        let mut turn = ConversationTurn {
            started_at: None,
            completed_at: None,
            id: "turn".into(),
            status: "unknown".into(),
            created_at: "1000".into(),
            updated_at: "1000".into(),
            items: vec![],
            running_elsewhere: false,
            edited_files: None,
        };
        upsert_conversation_thread_item_at(&mut turn, running, 1);
        upsert_conversation_thread_item_at(&mut turn, hydrated, 3);
        assert_eq!(turn.items.len(), 1);
        assert_eq!(turn.items[0].state, "started");

        let completed =
            typed_thread_item_projection_with_lifecycle(&entry, "3000", None, Some("completed"), 2);
        upsert_conversation_thread_item_at(&mut turn, completed, 1);
        upsert_conversation_thread_item_at(
            &mut turn,
            typed_thread_item_projection(&entry, "4000", None),
            3,
        );
        assert_eq!(turn.items[0].state, "completed");
        assert_eq!(turn.items[0].id, "compact-1");
    }

    #[test]
    fn compactions_keep_multiple_item_ids_and_terminal_turn_states() {
        fn upsert_detail(id: &str, state: &str) -> String {
            serde_json::json!({
                "turnId": "turn",
                "itemId": id,
                "state": state,
                "lifecycleAuthority": 2,
                "item": {"id": id, "type": "contextCompaction"}
            })
            .to_string()
        }
        fn envelope(sequence: u64, event: WonderEvent) -> HostEventEnvelope {
            HostEventEnvelope {
                event_id: format!("event-{sequence}"),
                host_epoch: "epoch".into(),
                sequence,
                occurred_at: format!("2026-09-12T00:00:0{sequence}Z"),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation".into()),
                message_id: None,
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: None,
                approval_id: None,
                event,
            }
        }

        let events = vec![
            envelope(
                1,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(upsert_detail("compact-1", "started")),
                },
            ),
            envelope(
                2,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(upsert_detail("compact-2", "started")),
                },
            ),
            envelope(
                3,
                WonderEvent::Activity {
                    category: "thread/compacted".into(),
                    state: "completed".into(),
                    detail: None,
                },
            ),
            envelope(
                4,
                WonderEvent::MessageState {
                    state: DeliveryState::Failed,
                },
            ),
            // Replay of the same canonical item must update it, not add a
            // second marker or merge the two actual compactions.
            envelope(
                5,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(upsert_detail("compact-1", "completed")),
                },
            ),
        ];
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &events,
            &[
                AppServerThreadItem {
                    turn_id: "turn".into(),
                    item: serde_json::json!({"id":"compact-1", "type":"contextCompaction"}),
                },
                AppServerThreadItem {
                    turn_id: "turn".into(),
                    item: serde_json::json!({"id":"compact-2", "type":"contextCompaction"}),
                },
            ],
            None,
        );
        let items = &projection.turns[0].items;
        assert_eq!(projection.turns[0].status, "failed");
        assert_eq!(
            items
                .iter()
                .filter(|item| item.item_type == "contextCompaction")
                .count(),
            2
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["compact-1", "compact-2"]
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.state.as_str())
                .collect::<Vec<_>>(),
            ["completed", "failed"]
        );
    }

    #[test]
    fn legacy_lifecycle_and_late_statusless_hydration_reconcile_truthfully() {
        fn envelope(sequence: u64, event: WonderEvent) -> HostEventEnvelope {
            HostEventEnvelope {
                event_id: format!("event-{sequence}"),
                host_epoch: "epoch".into(),
                sequence,
                occurred_at: format!("2026-09-12T00:00:0{sequence}Z"),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation".into()),
                message_id: None,
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: None,
                approval_id: None,
                event,
            }
        }

        let legacy_started = serde_json::json!({
            "turnId": "turn",
            "item": {"id": "compact-legacy", "type": "contextCompaction"},
            "state": "started"
        })
        .to_string();
        let late_hydration = serde_json::json!({
            "turnId": "turn",
            "item": {"id": "compact-late", "type": "contextCompaction"},
            "state": "unknown",
            "historyRefresh": true
        })
        .to_string();
        let legacy_projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[envelope(
                1,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(legacy_started),
                },
            )],
            &[],
            None,
        );
        assert_eq!(legacy_projection.turns[0].items.len(), 1);
        assert_eq!(legacy_projection.turns[0].items[0].state, "started");

        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[
                envelope(
                    2,
                    WonderEvent::MessageState {
                        state: DeliveryState::Failed,
                    },
                ),
                envelope(
                    3,
                    WonderEvent::Activity {
                        category: "thread_item_upsert".into(),
                        state: "updated".into(),
                        detail: Some(late_hydration),
                    },
                ),
            ],
            &[],
            None,
        );
        let items = &projection.turns[0].items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, "unknown");

        let interrupted = serde_json::json!({
            "turnId": "turn",
            "item": {"id": "compact-interrupted", "type": "contextCompaction"},
            "state": "unknown",
            "historyRefresh": true
        })
        .to_string();
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[
                envelope(
                    4,
                    WonderEvent::MessageState {
                        state: DeliveryState::Interrupted,
                    },
                ),
                envelope(
                    5,
                    WonderEvent::Activity {
                        category: "thread_item_upsert".into(),
                        state: "updated".into(),
                        detail: Some(interrupted),
                    },
                ),
            ],
            &[],
            None,
        );
        assert_eq!(projection.turns[0].items[0].state, "unknown");
    }
}
