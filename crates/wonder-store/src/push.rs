use super::*;

#[derive(Debug)]
pub struct PushDelivery {
    pub id: String,
    pub registration_id: String,
    pub endpoint: String,
    pub sender_secret: String,
    pub route_id: String,
    pub kind: String,
    pub attempts: i64,
    pub preview_key: Option<String>,
    pub source_key: String,
    pub source_conversation: String,
    pub conversation_id: String,
    // Persisted next_attempt is also the claim's lease identity. A replacement
    // claim can only be taken after it expires and gets a later deadline.
    claim_until: i64,
}

#[derive(Debug)]
pub struct PushAttempt {
    pub attempts: i64,
    id: String,
    claim_until: i64,
    registration_id: String,
    endpoint: String,
    sender_secret: String,
}

#[derive(Debug)]
pub struct PushRevocation {
    pub id: String,
    pub endpoint: String,
    pub sender_secret: String,
}

/// How long an approval may wait for its turn to be mapped to a conversation.
const APPROVAL_ROUTING_GRACE_SECONDS: i64 = 120;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPreview {
    pub title: String,
    pub body: String,
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<PushApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<PushApproval>,
    /// The history position a finished reply sits at. The phone removes the
    /// notification once a read acknowledgement for this host covers it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_through: Option<PushReadThrough>,
    /// Set only on a notification an agent sent on purpose (`wonder_notify`):
    /// what a tap opens. It never carries an action beyond opening.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<PushOpen>,
}

/// A tap opens the chat, or one of its Project's files in the chat that has it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PushOpen {
    Thread,
    #[serde(rename_all = "camelCase")]
    File {
        file_id: String,
        conversation_id: String,
    },
}

/// A notification an agent asked for, before it is fanned out to phones.
pub struct AgentNotification<'a> {
    pub conversation_id: &'a str,
    pub client_request_id: &'a str,
    pub dedupe_key: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub open: PushOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentNotificationOutcome {
    /// A new notification is on its way to the owner's phones.
    Queued,
    /// One with the same dedupe key is still waiting to be sent; it now
    /// carries this content and no second notification is sent. Once that
    /// one has been sent, the same key queues a new notification instead.
    Coalesced,
    /// The chat already sent its allowance in the window; nothing was sent.
    RateLimited,
    /// No phone has Cupola notifications turned on; nothing was sent.
    NoPhone,
}

/// At most this many agent notifications per chat in `AGENT_NOTIFY_WINDOW_SECONDS`.
pub const AGENT_NOTIFY_LIMIT: i64 = 3;
pub const AGENT_NOTIFY_WINDOW_SECONDS: i64 = 600;
const AGENT_NOTIFY_PREFIX: &str = "notify:";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAgentNotification {
    title: String,
    body: String,
    dedupe_key: String,
    open: PushOpen,
    /// queued or coalesced: a coalesced call is kept only to answer retries.
    state: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushReadThrough {
    pub host_epoch: String,
    pub sequence: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushApproval {
    pub supports_deny_message: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_options: Option<Vec<PushQuestionOption>>,
    pub approval_id: String,
    pub action_nonce: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PushQuestion {
    pub id: String,
    pub body: String,
}

struct PushOptionQuestion {
    id: String,
    body: String,
    options: Vec<PushQuestionOption>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PushQuestionOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// Preserve the existing API capability: older phones require the complete
// display body, including all choices, within its original 382-byte budget.
pub fn notification_question(params: &serde_json::Value) -> Option<PushQuestion> {
    let form = notification_question_form(params)?;
    if form.options.len() > 4 {
        return None;
    }
    let mut parts = vec![form.body];
    parts.extend(form.options.iter().map(|option| match &option.description {
        Some(description) => format!("{}: {description}", option.label),
        None => option.label.clone(),
    }));
    let body = parts.join("\n");
    (body.len() <= 382).then_some(PushQuestion { id: form.id, body })
}

fn notification_option_question(params: &serde_json::Value) -> Option<PushOptionQuestion> {
    let form = notification_question_form(params)?;
    let unique: std::collections::HashSet<_> =
        form.options.iter().map(|option| &option.label).collect();
    (!form.options.is_empty() && unique.len() == form.options.len()).then_some(form)
}

// One parser owns the complete eligible form. New encrypted question routes
// carry its prompt and every option separately instead of changing the old API.
fn notification_question_form(params: &serde_json::Value) -> Option<PushOptionQuestion> {
    let fields = params.as_object()?;
    if params["_wonderSupportsQuestionReply"] != true
        || params["isBlocking"].as_bool().is_none()
        || params.get("kind").is_some_and(|value| value != "question")
        || fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "threadId"
                    | "turnId"
                    | "itemId"
                    | "kind"
                    | "agentFamily"
                    | "questions"
                    | "isBlocking"
                    | "autoResolutionMs"
                    | "_wonderRuntimeId"
                    | "_wonderRequestId"
                    | "_wonderQuestionDeadlineMs"
                    | "_wonderSupportsQuestionReply"
            )
        })
    {
        return None;
    }
    let questions = params["questions"].as_array()?;
    if questions.len() != 1 {
        return None;
    }
    let question = questions[0].as_object()?;
    if question.keys().any(|key| {
        !matches!(
            key.as_str(),
            "id" | "header" | "question" | "options" | "isOther" | "isSecret" | "multiSelect"
        )
    }) || question.get("isOther") != Some(&serde_json::json!(true))
        || question.get("isSecret").is_some_and(|value| value != false)
        || question
            .get("multiSelect")
            .is_some_and(|value| value != false)
    {
        return None;
    }
    let text = |value: &str, limit| {
        !value.trim().is_empty()
            && value.len() <= limit
            && !value.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
    };
    let id = question.get("id")?.as_str()?;
    for field in ["threadId", "turnId", "itemId"] {
        let value = params[field].as_str()?;
        if !text(value, 256) || value.chars().any(char::is_whitespace) {
            return None;
        }
    }
    if !text(id, 256) || id.chars().any(char::is_whitespace) {
        return None;
    }
    let prompt = question.get("question")?.as_str()?;
    if !text(prompt, 300) {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(header) = question.get("header").filter(|value| !value.is_null()) {
        let header = header.as_str()?;
        if !header.is_empty() {
            if !text(header, 80) {
                return None;
            }
            parts.push(header.to_owned());
        }
    }
    parts.push(prompt.to_owned());
    let mut choices = Vec::new();
    if let Some(options) = question.get("options").filter(|value| !value.is_null()) {
        let options = options.as_array()?;
        // Keep the entire list. The encrypted APNs envelope has its own byte
        // budget; never truncate a question into a different set of choices.
        if options.len() > 32 {
            return None;
        }
        for option in options {
            let option = option.as_object()?;
            if option
                .keys()
                .any(|key| !matches!(key.as_str(), "label" | "description"))
            {
                return None;
            }
            let label = option.get("label")?.as_str()?;
            if !text(label, 120) {
                return None;
            }
            let description = match option.get("description").filter(|value| !value.is_null()) {
                Some(value) => value.as_str()?,
                None => "",
            };
            if !description.is_empty() && !text(description, 160) {
                return None;
            }
            choices.push(PushQuestionOption {
                label: label.to_owned(),
                description: (!description.is_empty()).then(|| description.to_owned()),
            });
        }
    }
    let body = parts.join("\n");
    (body.len() <= 382).then(|| PushOptionQuestion {
        id: id.to_owned(),
        body,
        options: choices,
    })
}

// A short, complete command can be reviewed in the expanded notification.
// Unknown or richer consent stays in the app, including stdin and file changes.
fn notification_command(approval: &StoredApproval) -> Option<String> {
    let params: serde_json::Value = serde_json::from_str(&approval.params_json).ok()?;
    let fields = params.as_object()?;
    if approval.state != "pending"
        || approval.method != "item/commandExecution/requestApproval"
        || params["kind"] != "command"
        || approval.action_nonce.is_empty()
        || fields.iter().any(|(key, value)| {
            !value.is_null()
                && !matches!(
                    key.as_str(),
                    "threadId"
                        | "turnId"
                        | "itemId"
                        | "approvalId"
                        | "kind"
                        | "command"
                        | "cwd"
                        | "reason"
                        | "commandActions"
                        | "availableDecisions"
                        | "startedAtMs"
                        | "agentFamily"
                        | "supportsDenyMessage"
                        | "_wonderRuntimeId"
                        | "_wonderRequestId"
                )
        })
    {
        return None;
    }
    if let Some(choices) = params.get("availableDecisions").filter(|v| !v.is_null()) {
        let choices = choices.as_array()?;
        if !choices.contains(&serde_json::json!("accept"))
            || !choices.contains(&serde_json::json!("decline"))
        {
            return None;
        }
        // These alternatives do not change the notification's one-time
        // accept/decline actions. Remembered choices stay inside the app and
        // are validated against the authenticated provider at resolution.
        let mut remembered_ids = std::collections::HashSet::new();
        for choice in choices {
            if matches!(
                choice.as_str(),
                Some("accept" | "decline" | "cancel" | "acceptForSession")
            ) {
                continue;
            }
            let object = choice.as_object()?;
            if object.len() != 1 {
                return None;
            }
            let key = match params["agentFamily"].as_str()? {
                "opencode" => "opencodePermission",
                "cursor" => "acpPermission",
                _ => return None,
            };
            let option = object.get(key)?.as_object()?;
            if option.len() != 2 {
                return None;
            }
            let id = option.get("optionId")?.as_str()?;
            let kind = option.get("kind")?.as_str()?;
            if id.trim().is_empty()
                || !matches!(kind, "allow_always" | "reject_always")
                || !remembered_ids.insert(id)
            {
                return None;
            }
        }
    }
    let command = params["command"].as_str()?;
    let cwd = params["cwd"].as_str()?;
    let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    if command.trim().is_empty()
        || cwd.trim().is_empty()
        || command.len() > 120
        || cwd.len() > 100
        || reason.len() > 160
        || [command, cwd, reason].iter().any(|v| {
            v.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
        })
    {
        return None;
    }
    Some(
        [reason, command, cwd]
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

// Bound UTF-8 bytes before encryption, including JSON's worst-case escaping.
fn excerpt(value: &str, budget: usize) -> String {
    let mut result = String::new();
    for ch in value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
    {
        if result.len() + ch.len_utf8() > budget {
            result.push('…');
            break;
        }
        result.push(ch);
    }
    result
}

/// A finished reply as notification text: Markdown structure becomes plain
/// lines and emphasis, code and link markup is removed, while the words, code
/// contents and anything that is not well-formed markup stay as written.
/// Only replies use this; approval and question bodies stay byte-identical.
fn plain_reply(text: &str) -> String {
    // Callers bound replies to 1000 characters; this keeps the cost bounded
    // for any other caller too.
    let text: String = text.chars().take(4000).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    // An indented code block starts after a blank line (or another code
    // line), never inside a paragraph or a list item.
    let mut after_blank = true;
    let mut in_code = false;
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = line[..line.len() - trimmed.len()]
            .chars()
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum::<usize>();
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = marker.map_or(0, |m| trimmed.chars().take_while(|c| *c == m).count());
        if let Some((open, length)) = fence {
            if marker == Some(open) && run >= length && trimmed[run..].trim().is_empty() {
                fence = None;
            } else {
                lines.push(line.trim_end().to_owned());
            }
            continue;
        }
        if trimmed.is_empty() {
            after_blank = true;
            lines.push(String::new());
            continue;
        }
        let was_blank = std::mem::replace(&mut after_blank, false);
        if indent >= 4 && (was_blank || in_code) && !in_list {
            in_code = true;
            lines.push(trimmed.trim_end().to_owned());
            continue;
        }
        in_code = false;
        let ordered = trimmed.split_once(['.', ')']).is_some_and(|(n, rest)| {
            (1..=9).contains(&n.len())
                && n.chars().all(|c| c.is_ascii_digit())
                && rest.starts_with(' ')
        });
        let bullet = ["- ", "* ", "+ "].iter().any(|b| trimmed.starts_with(b));
        if bullet || ordered {
            in_list = true;
        } else if indent < 2 && was_blank {
            in_list = false;
        }
        if indent <= 3 && run >= 3 && !(marker == Some('`') && trimmed[run..].contains('`')) {
            fence = marker.map(|m| (m, run));
            continue;
        }
        let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        let rule = |c: char| compact.len() >= 3 && compact.chars().all(|x| x == c);
        if indent <= 3 && (rule('-') || rule('*') || rule('_') || rule('=')) {
            continue;
        }
        if trimmed.starts_with('|')
            && compact.contains('-')
            && compact.chars().all(|c| matches!(c, '|' | '-' | ':'))
        {
            continue;
        }
        let mut rest = trimmed;
        while let Some(quoted) = rest.strip_prefix('>') {
            rest = quoted.strip_prefix(' ').unwrap_or(quoted);
        }
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        let mut prefix = "";
        if (1..=6).contains(&hashes) && rest[hashes..].starts_with([' ', '\t']) {
            // A closing run of #s counts only after a space: "C#" keeps its #.
            let heading = rest[hashes..].trim();
            let open = heading.trim_end_matches('#');
            rest = if open.is_empty() || open.ends_with([' ', '\t']) {
                open.trim_end()
            } else {
                heading
            };
        } else if let Some(item) = ["- ", "* ", "+ "].iter().find_map(|b| rest.strip_prefix(b)) {
            prefix = "• ";
            rest = item.trim_start();
        }
        let line =
            if rest.starts_with('|') && rest.trim_end().ends_with('|') && rest.trim_end().len() > 1
            {
                let row = rest.trim_end();
                row[1..row.len() - 1]
                    .split('|')
                    .map(|cell| plain_inline(cell.trim()))
                    .collect::<Vec<_>>()
                    .join(" · ")
            } else {
                plain_inline(rest.trim_end())
            };
        lines.push(format!("{prefix}{line}"));
    }
    // Paragraphs stay apart by one blank line, however many the reply used.
    let mut result = String::new();
    let mut blank = false;
    for line in lines {
        if line.trim().is_empty() {
            blank = true;
            continue;
        }
        if !result.is_empty() {
            result.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        result.push_str(&line);
    }
    let result = result.trim();
    // A reply made only of markup keeps its own text.
    if result.is_empty() {
        text.trim().to_owned()
    } else {
        result.to_owned()
    }
}

/// Inline Markdown: code spans, emphasis, strikethrough, links, images,
/// autolinks and backslash escapes. Unmatched delimiters stay literal.
fn plain_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let run_at = |at: usize, c: char| chars[at..].iter().take_while(|x| **x == c).count();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '`' => {
                let length = run_at(i, '`');
                let mut j = i + length;
                while j < chars.len() {
                    let close = run_at(j, '`');
                    if close == length {
                        let code: String = chars[i + length..j].iter().collect();
                        let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ')
                        {
                            &code[1..code.len() - 1]
                        } else {
                            &code
                        };
                        out.push_str(code);
                        i = j + length;
                        break;
                    }
                    j += close.max(1);
                }
                if j >= chars.len() {
                    out.extend(&chars[i..i + length]);
                    i += length;
                }
                continue;
            }
            '!' | '[' => {
                let open = if c == '!' { i + 1 } else { i };
                if chars.get(open) == Some(&'[') {
                    if let Some((label, end)) = link_at(&chars, open) {
                        out.push_str(&plain_inline(&label));
                        i = end;
                        continue;
                    }
                }
            }
            '<' => {
                if let Some(close) = chars[i + 1..].iter().position(|x| *x == '>') {
                    let inner: String = chars[i + 1..i + 1 + close].iter().collect();
                    if !inner.contains(char::is_whitespace)
                        && (inner.contains("://") || inner.starts_with("mailto:"))
                    {
                        out.push_str(&inner);
                        i += close + 2;
                        continue;
                    }
                }
            }
            '*' | '_' | '~' => {
                let length = run_at(i, c);
                let before = i.checked_sub(1).map(|b| chars[b]);
                let after = chars.get(i + length).copied();
                let opens = after.is_some_and(|a| !a.is_whitespace())
                    && (c != '_' || !before.is_some_and(char::is_alphanumeric))
                    && (c != '~' || length == 2)
                    && length <= 3;
                let close = opens.then(|| {
                    (i + length + 1..chars.len()).find(|&j| {
                        run_at(j, c) == length
                            && chars[j - 1] != c
                            && !chars[j - 1].is_whitespace()
                            && (c != '_'
                                || !chars.get(j + length).is_some_and(|x| x.is_alphanumeric()))
                    })
                });
                if let Some(Some(j)) = close {
                    let inner: String = chars[i + length..j].iter().collect();
                    out.push_str(&plain_inline(&inner));
                    i = j + length;
                } else {
                    out.extend(&chars[i..i + length]);
                    i += length;
                }
                continue;
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `[label](destination)` starting at `open`: the label and the index after it.
fn link_at(chars: &[char], open: usize) -> Option<(String, usize)> {
    let mut depth = 0;
    let mut close = None;
    for (offset, c) in chars[open..].iter().enumerate() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..].iter().position(|c| *c == ')')? + close + 2;
    if chars[close + 2..end].contains(&'\n') {
        return None;
    }
    Some((chars[open + 1..close].iter().collect(), end + 1))
}

fn question_text(value: &serde_json::Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(3)
        .filter_map(|q| {
            q.get("title")
                .or_else(|| q.get("question"))
                .and_then(|v| v.as_str())
        })
        .map(|v| excerpt(v, 300))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The type of the reply or tool call a turn started last, from its live
/// item events (`thread_item_upsert`). Thinking, the prompt and suggestions
/// are not steps.
const LAST_STEP_SQL: &str = "SELECT json_extract(json_extract(payload_json,'$.event.data.detail'),'$.itemType') FROM events WHERE json_extract(payload_json,'$.conversationId')=? AND json_extract(payload_json,'$.turnId')=? AND json_extract(payload_json,'$.event.type')='activity' AND json_extract(payload_json,'$.event.data.category')='thread_item_upsert' AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.itemType') IN ('agentMessage','commandExecution','fileChange','mcpToolCall','dynamicToolCall','subAgentActivity','collabAgentToolCall','webSearch','imageView','imageGeneration','taskChecklist','functionCallOutput') GROUP BY json_extract(json_extract(payload_json,'$.event.data.detail'),'$.itemId') ORDER BY MIN(rowid) DESC LIMIT 1";

fn approval_preview(params: &serde_json::Value) -> (&'static str, String) {
    let questions = question_text(&params["questions"]);
    if !questions.is_empty() {
        return ("Question", questions);
    }
    let mut parts = Vec::new();
    for key in [
        "message",
        "reason",
        "command",
        "filePath",
        "grantRoot",
        "url",
    ] {
        if let Some(text) = params[key].as_str().filter(|v| !v.trim().is_empty()) {
            let text = excerpt(text, 600);
            if !parts.contains(&text) {
                parts.push(text);
            }
        }
    }
    ("Approval requested", parts.join("\n"))
}

impl Store {
    /// Resolve the exact triggering turn/request; never use another turn's latest reply.
    pub async fn push_preview(
        &self,
        delivery: &PushDelivery,
    ) -> Result<Option<PushPreview>, sqlx::Error> {
        let title: String=sqlx::query_scalar("SELECT COALESCE((SELECT NULLIF(trim(title),'') FROM project_conversations WHERE conversation_id=?),'Cupola')")
            .bind(&delivery.conversation_id).fetch_one(&self.pool).await?;
        if delivery.source_key.starts_with(AGENT_NOTIFY_PREFIX) {
            return self.agent_notification_preview(delivery, &title).await;
        }
        let mut action = None;
        let mut read_through = None;
        let (label, mut body) = if let Some(external) =
            delivery.source_key.strip_prefix("external:")
        {
            let body: Option<String> =
                sqlx::query_scalar("SELECT preview_body FROM push_intents WHERE source_key=?")
                    .bind(&delivery.source_key)
                    .fetch_optional(&self.pool)
                    .await?
                    .flatten();
            let agent = if external.starts_with("claude:") {
                "Claude"
            } else {
                "Codex"
            };
            // Window observation belongs to the computer, never to this turn.
            if external.starts_with("mac-permission:") {
                (
                    "A Mac dialog needs you",
                    "Open Computer in Cupola to view the dialog, or review it on your Mac."
                        .to_owned(),
                )
            // The source key ends with the outcome (external_push.rs).
            } else if external.ends_with(":failed") {
                (
                    "Needs attention",
                    format!(
                        "{agent} could not finish this task on your computer. Open Cupola to review it."
                    ),
                )
            } else if external.ends_with(":ended") {
                (
                    "Needs attention",
                    format!("{agent} stopped on your computer, but Cupola couldn't tell whether the task finished. Open Cupola to review it."),
                )
            } else if delivery.kind == "attention" {
                (
                    "Waiting on your computer",
                    format!("{agent} is asking for permission or an answer on your computer. Respond there to continue."),
                )
            } else {
                // The reply is in the agent's own history, not a Cupola
                // event: it is read once the phone has seen everything
                // the host had recorded when this notification was made.
                read_through = sqlx::query_as::<_, (String, i64)>(
                    "SELECT host_epoch,last_sequence FROM sync_state WHERE singleton=1",
                )
                .fetch_optional(&self.pool)
                .await?
                .and_then(|(host_epoch, sequence)| {
                    Some(PushReadThrough {
                        host_epoch,
                        sequence: u64::try_from(sequence).ok()?,
                    })
                });
                (
                    "",
                    body.map(|body| plain_reply(&body))
                        .unwrap_or_else(|| format!("{agent} finished replying on your computer.")),
                )
            }
        } else if let Some(request) = delivery.source_key.strip_prefix("approval:") {
            let value: Option<String> = sqlx::query_scalar(
                "SELECT params_json FROM approvals WHERE server_request_id=? AND state='pending'",
            )
            .bind(request)
            .fetch_optional(&self.pool)
            .await?;
            let Some(value) = value else {
                return Ok(None);
            };
            if let Some(approval) = self.approval(request).await? {
                if self
                    .approval_push_conversation(&approval.thread_id, &approval.turn_id)
                    .await?
                    .as_deref()
                    == Some(&delivery.conversation_id)
                {
                    if let Some(command) = notification_command(&approval) {
                        action = Some((
                            PushApproval {
                                question_id: None,
                                question_options: None,
                                supports_deny_message: serde_json::from_str::<serde_json::Value>(
                                    &approval.params_json,
                                )
                                .is_ok_and(|params| params["supportsDenyMessage"] == true),
                                approval_id: approval.approval_id,
                                action_nonce: approval.action_nonce,
                            },
                            command,
                        ));
                    } else if approval.state == "pending"
                        && approval.method == "item/tool/requestUserInput"
                        && !approval.action_nonce.is_empty()
                    {
                        if let Some(question) = serde_json::from_str(&approval.params_json)
                            .ok()
                            .as_ref()
                            .and_then(notification_option_question)
                        {
                            action = Some((
                                PushApproval {
                                    supports_deny_message: false,
                                    question_id: Some(question.id),
                                    question_options: Some(question.options),
                                    approval_id: approval.approval_id,
                                    action_nonce: approval.action_nonce,
                                },
                                question.body,
                            ));
                        }
                    }
                }
            }
            approval_preview(&serde_json::from_str(&value).unwrap_or_default())
        } else if let Some(question) = delivery.source_key.strip_prefix("question:") {
            let value:Option<String>=sqlx::query_scalar("SELECT questions_json FROM async_questions WHERE id=? AND conversation_id=? AND state='pending' AND expires_at_ms>unixepoch('subsec')*1000")
                .bind(question).bind(&delivery.source_conversation).fetch_optional(&self.pool).await?;
            let Some(value) = value else {
                return Ok(None);
            };
            (
                "Question",
                question_text(&serde_json::from_str(&value).unwrap_or_default()),
            )
        } else {
            let payload: Option<String> =
                sqlx::query_scalar("SELECT payload_json FROM events WHERE event_id=?")
                    .bind(&delivery.source_key)
                    .fetch_optional(&self.pool)
                    .await?;
            let Some(event) =
                payload.and_then(|p| serde_json::from_str::<HostEventEnvelope>(&p).ok())
            else {
                return Ok(None);
            };
            if event.conversation_id.as_deref() != Some(&delivery.source_conversation) {
                return Ok(None);
            }
            match event.event {
                WonderEvent::ApprovalOpened { request_id } => {
                    let value:Option<String>=sqlx::query_scalar("SELECT params_json FROM approvals WHERE server_request_id=? AND thread_id=? AND turn_id=? AND state='pending'")
                        .bind(request_id).bind(event.thread_id.as_deref().unwrap_or("")).bind(event.turn_id.as_deref().unwrap_or("")).fetch_optional(&self.pool).await?;
                    let Some(value) = value else {
                        return Ok(None);
                    };
                    approval_preview(&serde_json::from_str(&value).unwrap_or_default())
                }
                WonderEvent::MessageState {
                    state: wonder_api::DeliveryState::Completed,
                } => {
                    let turn = event.turn_id.as_deref().unwrap_or("");
                    // A turn whose last step was a tool call, with no text
                    // after it, has no final reply: the text before that call
                    // was commentary.
                    let last_step: Option<String> = sqlx::query_scalar(LAST_STEP_SQL)
                        .bind(&delivery.source_conversation)
                        .bind(turn)
                        .fetch_optional(&self.pool)
                        .await?;
                    let text: Option<String> = if last_step
                        .as_deref()
                        .is_some_and(|step| step != "agentMessage")
                    {
                        None
                    } else {
                        sqlx::query_scalar("SELECT substr(text,1,1000) FROM assistant_messages WHERE conversation_id=? AND codex_thread_id=? AND codex_turn_id=? AND state='completed' AND trim(text)!='' ORDER BY created_at DESC,rowid DESC LIMIT 1")
                            .bind(&delivery.source_conversation).bind(event.thread_id.as_deref().unwrap_or("")).bind(turn).fetch_optional(&self.pool).await?
                    };
                    read_through = Some(PushReadThrough {
                        host_epoch: event.host_epoch.clone(),
                        sequence: event.sequence,
                    });
                    (
                        "",
                        text.map_or_else(|| "A reply finished.".into(), |text| plain_reply(&text)),
                    )
                }
                WonderEvent::MessageState {
                    state: wonder_api::DeliveryState::Uncertain,
                } => (
                    "Needs attention",
                    "This task ended, but Cupola couldn't tell how. Open Cupola to review it."
                        .into(),
                ),
                _ => (
                    "Needs attention",
                    "This task could not finish. Open Cupola to review it.".into(),
                ),
            }
        };
        let mac_dialog = delivery.source_key.starts_with("external:mac-permission:");
        let title = if mac_dialog {
            label.to_owned()
        } else if label.is_empty() {
            title
        } else {
            format!("{} · {label}", excerpt(&title, 100))
        };
        let approval = action.map(|(reference, complete_body)| {
            body = complete_body;
            reference
        });
        let body = if approval
            .as_ref()
            .is_some_and(|reference| reference.question_id.is_some())
        {
            body
        } else {
            excerpt(
                if body.trim().is_empty() {
                    "Open Cupola to review this request."
                } else {
                    &body
                },
                1000,
            )
        };
        // Older phones understand approval actions but cannot display an
        // option list. A separate field leaves those notifications app-opening.
        let (approval, question) = if approval.as_ref().is_some_and(|a| a.question_id.is_some()) {
            (None, approval)
        } else {
            (approval, None)
        };
        Ok(Some(PushPreview {
            conversation_id: if mac_dialog {
                crate::HOST_VIEW_CONVERSATION_ID.into()
            } else {
                delivery.conversation_id.clone()
            },
            approval,
            question,
            read_through,
            title: excerpt(&title, 160),
            body,
            open: None,
        }))
    }

    /// An agent's own notification, read when it is sent: the latest content
    /// coalesced into it, and a file target only while that file is still
    /// one of the Project's outputs; otherwise a tap opens the chat.
    async fn agent_notification_preview(
        &self,
        delivery: &PushDelivery,
        chat: &str,
    ) -> Result<Option<PushPreview>, sqlx::Error> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT preview_body FROM push_intents WHERE source_key=?")
                .bind(&delivery.source_key)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let Some(notification) =
            stored.and_then(|stored| serde_json::from_str::<StoredAgentNotification>(&stored).ok())
        else {
            return Ok(None);
        };
        let open = match notification.open {
            PushOpen::File {
                file_id,
                conversation_id,
            } if self
                .agent_notification_file(&delivery.source_conversation, &file_id)
                .await?
                .as_deref()
                == Some(conversation_id.as_str()) =>
            {
                PushOpen::File {
                    file_id,
                    conversation_id,
                }
            }
            _ => PushOpen::Thread,
        };
        Ok(Some(PushPreview {
            title: excerpt(
                &format!("{} · {}", excerpt(chat, 100), notification.title),
                160,
            ),
            body: excerpt(&notification.body, 1000),
            conversation_id: delivery.conversation_id.clone(),
            approval: None,
            question: None,
            read_through: None,
            open: Some(open),
        }))
    }

    /// The chat holding `file_id` when it is an output of the same Project
    /// as `conversation_id`.
    pub async fn agent_notification_file(
        &self,
        conversation_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT f.conversation_id FROM conversation_files f JOIN project_conversations c ON c.conversation_id=f.conversation_id JOIN project_conversations me ON me.project_id=c.project_id WHERE f.id=? AND me.conversation_id=? AND f.kind IN ('attachment','artifact')")
            .bind(file_id)
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Record a notification an agent sent on purpose, through the same
    /// intents and outbox as every other push. A retry with the same request
    /// ID returns the first answer. One with a dedupe key whose notification
    /// is still waiting to be sent updates it instead of sending another, and a
    /// chat sends at most `AGENT_NOTIFY_LIMIT` per window.
    pub async fn enqueue_agent_notification(
        &self,
        notification: AgentNotification<'_>,
    ) -> Result<AgentNotificationOutcome, sqlx::Error> {
        let key = format!(
            "{AGENT_NOTIFY_PREFIX}{}:{}",
            notification.conversation_id, notification.client_request_id
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let earlier: Option<String> =
            sqlx::query_scalar("SELECT preview_body FROM push_intents WHERE source_key=?")
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        if let Some(earlier) = earlier {
            let state = serde_json::from_str::<StoredAgentNotification>(&earlier)
                .map(|stored| stored.state)
                .unwrap_or_default();
            return Ok(if state == "coalesced" {
                AgentNotificationOutcome::Coalesced
            } else {
                AgentNotificationOutcome::Queued
            });
        }
        let phones: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)")
                .fetch_one(&mut *tx)
                .await?;
        if !phones {
            return Ok(AgentNotificationOutcome::NoPhone);
        }
        let recent: Vec<(i64, String)> = sqlx::query_as("SELECT id,preview_body FROM push_intents WHERE substr(source_key,1,length(?1))=?1 AND conversation_id=?2 AND created_at>=unixepoch()-?3 AND preview_body IS NOT NULL ORDER BY id DESC")
            .bind(AGENT_NOTIFY_PREFIX)
            .bind(notification.conversation_id)
            .bind(AGENT_NOTIFY_WINDOW_SECONDS)
            .fetch_all(&mut *tx)
            .await?;
        let recent: Vec<(i64, StoredAgentNotification)> = recent
            .into_iter()
            .filter_map(|(id, body)| Some((id, serde_json::from_str(&body).ok()?)))
            .filter(|(_, stored): &(i64, StoredAgentNotification)| stored.state == "queued")
            .collect();
        let mut stored = StoredAgentNotification {
            title: notification.title.to_owned(),
            body: notification.body.to_owned(),
            dedupe_key: notification.dedupe_key.to_owned(),
            open: notification.open,
            state: "queued".into(),
        };
        // The newest notification with this key, while it still waits to be
        // sent: not yet fanned out, or with a send to some phone pending.
        // Once it has gone out, an update is news and sends a new push.
        let mut waiting = None;
        if let Some((id, _)) = recent
            .iter()
            .find(|(_, earlier)| earlier.dedupe_key == notification.dedupe_key)
        {
            let unsent: bool = sqlx::query_scalar("SELECT distributed=0 OR EXISTS(SELECT 1 FROM push_outbox WHERE intent_id=?1 AND state='pending') FROM push_intents WHERE id=?1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            waiting = unsent.then_some(*id);
        }
        let outcome = if let Some(id) = waiting {
            // The pending send carries the newest content.
            sqlx::query("UPDATE push_intents SET preview_body=? WHERE id=?")
                .bind(serde_json::to_string(&stored).unwrap_or_default())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            stored.state = "coalesced".into();
            AgentNotificationOutcome::Coalesced
        } else if recent.len() as i64 >= AGENT_NOTIFY_LIMIT {
            return Ok(AgentNotificationOutcome::RateLimited);
        } else {
            AgentNotificationOutcome::Queued
        };
        // A coalesced call is recorded already distributed, so it never
        // sends and only answers its own retries.
        sqlx::query("INSERT INTO push_intents(source_key,conversation_id,kind,preview_body,distributed) VALUES(?,?,'attention',?,?)")
            .bind(&key)
            .bind(notification.conversation_id)
            .bind(serde_json::to_string(&stored).unwrap_or_default())
            .bind(outcome == AgentNotificationOutcome::Coalesced)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }
    pub async fn push_enabled(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)")
            .fetch_one(&self.pool)
            .await
    }

    /// Project threads Wonder follows that are bound to a native Claude Code
    /// session or Codex thread: (conversation, family, native id).
    pub async fn followed_native_threads(
        &self,
    ) -> Result<Vec<(String, AgentFamily, String)>, sqlx::Error> {
        let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT c.conversation_id,c.agent_family,c.native_session_id FROM project_conversations c JOIN projects p ON p.id=c.project_id WHERE c.native_session_id IS NOT NULL AND c.wonder_archived_at IS NULL AND p.is_included=1")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(conversation, family, native)| {
                let family = match family.as_str() {
                    "claude" => AgentFamily::Claude,
                    "codex" => AgentFamily::Codex,
                    _ => return None,
                };
                Some((conversation, family, native))
            })
            .collect())
    }

    /// Record a notification for work done outside Wonder in a followed
    /// thread. The source key makes each turn or prompt notify once.
    pub async fn enqueue_external_push(
        &self,
        source_key: &str,
        conversation: &str,
        kind: &str,
        preview_body: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let inserted = sqlx::query("INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind,preview_body) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)")
            .bind(format!("external:{source_key}"))
            .bind(conversation)
            .bind(kind)
            .bind(preview_body.map(|body| excerpt(body, 1000)))
            .execute(&self.pool)
            .await?;
        Ok(inserted.rows_affected() == 1)
    }

    /// The reason for an external push ended before it was sent: cancel
    /// deliveries still pending and keep the intent from being fanned out.
    /// One already delivered stays delivered.
    pub async fn withdraw_external_push(&self, source_key: &str) -> Result<(), sqlx::Error> {
        let key = format!("external:{source_key}");
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE push_outbox SET state='cancelled' WHERE state='pending' AND intent_id=(SELECT id FROM push_intents WHERE source_key=?)")
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE push_intents SET distributed=1 WHERE source_key=?")
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    pub async fn register_push(
        &self,
        device: &str,
        id: &str,
        endpoint: &str,
        secret: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // A racing device revocation cannot restore delivery authority.
        let active: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked_at IS NULL AND forgotten=0 AND is_local=0)").bind(device).fetch_one(&mut *tx).await?;
        if !active {
            return Err(sqlx::Error::RowNotFound);
        }
        let owner: Option<String> =
            sqlx::query_scalar("SELECT device_id FROM push_registrations WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if owner.as_deref().is_some_and(|value| value != device) {
            return Err(sqlx::Error::RowNotFound);
        }
        sqlx::query("UPDATE push_registrations SET revoked=1 WHERE device_id=? AND id!=?")
            .bind(device)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE push_outbox SET state='cancelled' WHERE registration_id IN(SELECT id FROM push_registrations WHERE device_id=? AND revoked=1) AND state='pending'").bind(device).execute(&mut *tx).await?;
        let changed = sqlx::query("INSERT INTO push_registrations(id,device_id,endpoint,sender_secret) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET endpoint=excluded.endpoint,sender_secret=excluded.sender_secret WHERE push_registrations.revoked=0")
            .bind(id).bind(device).bind(endpoint).bind(secret).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        tx.commit().await
    }
    pub async fn register_push_preview(
        &self,
        device: &str,
        id: &str,
        key: &str,
    ) -> Result<(), sqlx::Error> {
        let result=sqlx::query("UPDATE push_registrations SET preview_key=? WHERE id=? AND device_id=? AND revoked=0 AND EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked_at IS NULL AND forgotten=0)")
            .bind(key).bind(id).bind(device).bind(device).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }
    pub async fn push_presence(
        &self,
        device: &str,
        foreground: bool,
        conversation: Option<&str>,
        issued: i64,
        now: i64,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let until = if foreground { now + 45_000 } else { 0 };
        let conversation = if foreground { conversation } else { Some("") };
        let changed=sqlx::query("INSERT INTO push_presence(device_id,issued_at_ms,foreground_until_ms,conversation_id) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked_at IS NULL AND forgotten=0 AND is_local=0) ON CONFLICT(device_id) DO UPDATE SET issued_at_ms=excluded.issued_at_ms,foreground_until_ms=excluded.foreground_until_ms,conversation_id=excluded.conversation_id WHERE excluded.issued_at_ms>push_presence.issued_at_ms")
            .bind(device).bind(issued).bind(until).bind(conversation).bind(device).execute(&mut *tx).await?;
        if foreground && changed.rows_affected() == 1 {
            // What arrived for the open conversation was seen on screen.
            sqlx::query("INSERT OR IGNORE INTO push_suppressed SELECT id,?1 FROM push_intents WHERE distributed=0 AND (?2 IS NULL OR (conversation_id!='' AND conversation_id=?2))").bind(device).bind(conversation).execute(&mut *tx).await?;
            sqlx::query("UPDATE push_outbox SET state='suppressed' WHERE state='pending' AND registration_id IN(SELECT id FROM push_registrations WHERE device_id=?1) AND (?2 IS NULL OR conversation_id=?2)").bind(device).bind(conversation).execute(&mut *tx).await?;
        }
        tx.commit().await
    }
    pub async fn revoke_push(&self, device: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE push_registrations SET revoked=1 WHERE device_id=?")
            .bind(device)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE push_outbox SET state='cancelled' WHERE registration_id IN(SELECT id FROM push_registrations WHERE device_id=?) AND state='pending'").bind(device).execute(&mut *tx).await?;
        tx.commit().await
    }
    pub async fn push_route(
        &self,
        device: &str,
        route: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT r.conversation_id FROM push_routes r JOIN devices d ON d.id=r.device_id WHERE r.id=? AND r.device_id=? AND d.revoked_at IS NULL")
            .bind(route).bind(device).fetch_optional(&self.pool).await
    }
    /// Exact turns and verified descendants carry their own ownership. Before
    /// a turn receipt exists, only a persisted dispatch currently submitting to
    /// this exact Project runtime authorizes routing an early approval.
    pub async fn approval_push_conversation(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        if let Some(conversation) = self.approval_conversation_id(thread, turn).await? {
            return Ok(self
                .owning_project_conversation(&conversation)
                .await?
                .map(|owner| owner.conversation_id));
        }
        if thread.is_empty() {
            return Ok(None);
        }
        // A question T3 Code asks in its thread (wonderd's `t3_code::questions`).
        if let Some(conversation) = self.t3_code_conversation(thread).await? {
            return Ok(self
                .owning_project_conversation(&conversation)
                .await?
                .map(|owner| owner.conversation_id));
        }
        // A request ChatGPT's desktop app asks in a thread it runs
        // (wonderd's `chatgpt_approvals`).
        if let Some(conversation) = self.chatgpt_request_conversation(thread, turn).await? {
            return Ok(self
                .owning_project_conversation(&conversation)
                .await?
                .map(|owner| owner.conversation_id));
        }
        let candidates: Vec<String> = sqlx::query_scalar("SELECT DISTINCT m.conversation_id FROM messages m JOIN dispatch_attempts d ON d.message_id=m.id AND d.id=(SELECT MAX(latest.id) FROM dispatch_attempts latest WHERE latest.message_id=m.id) WHERE m.codex_thread_id=? AND d.thread_id=m.codex_thread_id AND m.codex_turn_id IS NULL AND d.turn_id IS NULL AND m.state='dispatching_to_codex' AND d.phase='submitting' AND NOT EXISTS(SELECT 1 FROM messages active WHERE active.conversation_id=m.conversation_id AND active.id<>m.id AND active.state IN ('dispatching_to_codex','accepted_by_codex','streaming')) LIMIT 2")
            .bind(thread).fetch_all(&self.pool).await?;
        match candidates.as_slice() {
            [conversation]
                if self
                    .verified_project_runtime_owner(conversation, thread)
                    .await? =>
            {
                Ok(Some(conversation.clone()))
            }
            _ => Ok(None),
        }
    }

    /// Give approval intents their conversation. An unresolved request waits
    /// for its turn mapping; a request that is no longer pending is dropped.
    async fn resolve_approval_intents(&self) -> Result<(), sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id,substr(source_key,10) FROM push_intents WHERE distributed=0 AND conversation_id='' AND source_key LIKE 'approval:%' ORDER BY id LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?;
        for (id, request) in rows {
            let approval: Option<(String, String)> = sqlx::query_as(
                "SELECT thread_id,turn_id FROM approvals WHERE server_request_id=? AND state='pending'",
            )
            .bind(&request)
            .fetch_optional(&self.pool)
            .await?;
            let conversation = match &approval {
                Some((thread, turn)) => self.approval_push_conversation(thread, turn).await?,
                None => None,
            };
            if let Some(conversation) = conversation {
                sqlx::query(
                    "UPDATE push_intents SET conversation_id=? WHERE id=? AND conversation_id=''",
                )
                .bind(conversation)
                .bind(id)
                .execute(&self.pool)
                .await?;
            } else if approval.is_none() {
                sqlx::query("UPDATE push_intents SET distributed=1 WHERE id=?")
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            } else {
                sqlx::query(
                    "UPDATE push_intents SET distributed=1 WHERE id=? AND created_at<unixepoch()-?",
                )
                .bind(id)
                .bind(APPROVAL_ROUTING_GRACE_SECONDS)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    pub async fn distribute_push(&self) -> Result<(), sqlx::Error> {
        self.resolve_approval_intents().await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows =
            sqlx::query("SELECT * FROM push_intents WHERE distributed=0 AND conversation_id!='' ORDER BY id LIMIT 100")
                .fetch_all(&mut *tx)
                .await?;
        for row in rows {
            let id: i64 = row.get("id");
            let kind: String = row.get("kind");
            let source: String = row.get("source_key");
            let conversation: String = row.get("conversation_id");
            // Walk only verified persisted ownership. Cycles/depth overflow fail closed.
            let root: Option<String> = sqlx::query_scalar(questions::VERIFIED_SUBAGENT_ROOT_SQL)
                .bind(&conversation)
                .fetch_optional(&mut *tx)
                .await?;
            let mut target = root;
            if let Some(root) = &target {
                let level: String = sqlx::query_scalar(
                    "SELECT notification_level FROM project_conversations WHERE conversation_id=?",
                )
                .bind(root)
                .fetch_one(&mut *tx)
                .await?;
                if level == "mute"
                    || (kind == "completed" && root != &conversation && level != "follow")
                {
                    target = None;
                }
            }
            if let Some(request) = source.strip_prefix("approval:") {
                let pending: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM approvals WHERE server_request_id=? AND state='pending')",
                )
                .bind(request)
                .fetch_one(&mut *tx)
                .await?;
                if !pending {
                    target = None;
                }
            }
            if let Some(question) = source.strip_prefix("question:") {
                let pending: bool = sqlx::query_scalar(questions::scoped_questions!(" SELECT EXISTS(SELECT 1 FROM scoped_questions WHERE id=? AND visible_conversation IS NOT NULL AND state='pending' AND expires_at_ms>unixepoch()*1000)")).bind(question).fetch_one(&mut *tx).await?;
                if !pending {
                    target = None;
                }
            }
            if row.get::<i64, _>("created_at") + 86400
                < std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            {
                target = None;
            }
            if let Some(target) = target {
                let recipients:Vec<String>=sqlx::query_scalar("SELECT r.id FROM push_registrations r JOIN devices d ON d.id=r.device_id WHERE r.revoked=0 AND d.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM push_presence p WHERE p.device_id=d.id AND p.foreground_until_ms>unixepoch('subsec')*1000 AND (p.conversation_id IS NULL OR p.conversation_id=?)) AND NOT EXISTS(SELECT 1 FROM push_suppressed s WHERE s.intent_id=? AND s.device_id=d.id)").bind(&target).bind(id).fetch_all(&mut *tx).await?;
                for registration in recipients {
                    let route = uuid::Uuid::new_v4().to_string();
                    sqlx::query("INSERT INTO push_routes(id,device_id,conversation_id) SELECT ?,device_id,? FROM push_registrations WHERE id=?").bind(&route).bind(&target).bind(&registration).execute(&mut *tx).await?;
                    sqlx::query("INSERT OR IGNORE INTO push_outbox(id,registration_id,intent_id,route_id,conversation_id,kind) VALUES(?,?,?,?,?,?)")
                        .bind(uuid::Uuid::new_v4().to_string()).bind(registration).bind(id).bind(route).bind(&target).bind(&kind).execute(&mut *tx).await?;
                }
            }
            sqlx::query("UPDATE push_intents SET distributed=1 WHERE id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM push_routes WHERE created_at<unixepoch()-604800")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM push_intents WHERE created_at<unixepoch()-604800")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE push_outbox SET state='expired' WHERE state='pending' AND created_at<unixepoch()-86400").execute(&mut *tx).await?;
        tx.commit().await
    }
    pub async fn next_push(&self) -> Result<Option<PushDelivery>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // An interrupted final send must not leave an exhausted row reserved.
        sqlx::query("UPDATE push_outbox SET state='failed' WHERE state='pending' AND attempts>=5 AND next_attempt<=unixepoch()")
            .execute(&mut *tx)
            .await?;
        let row=sqlx::query("SELECT o.*,r.endpoint,r.sender_secret,r.preview_key,i.source_key,i.conversation_id AS source_conversation FROM push_outbox o JOIN push_intents i ON i.id=o.intent_id JOIN push_registrations r ON r.id=o.registration_id JOIN devices d ON d.id=r.device_id WHERE o.state='pending' AND o.attempts<5 AND o.next_attempt<=unixepoch() AND r.revoked=0 AND d.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM push_presence p WHERE p.device_id=d.id AND p.foreground_until_ms>unixepoch('subsec')*1000 AND (p.conversation_id IS NULL OR p.conversation_id=o.conversation_id)) ORDER BY o.created_at LIMIT 1").fetch_optional(&mut *tx).await?;
        let result = if let Some(row) = row {
            let id: String = row.get("id");
            let claim_until = sqlx::query_scalar(
                "UPDATE push_outbox SET next_attempt=unixepoch()+60 WHERE id=? RETURNING next_attempt",
            )
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
            Some(PushDelivery {
                id,
                registration_id: row.get("registration_id"),
                endpoint: row.get("endpoint"),
                sender_secret: row.get("sender_secret"),
                route_id: row.get("route_id"),
                kind: row.get("kind"),
                attempts: row.get("attempts"),
                claim_until,
                preview_key: row.get("preview_key"),
                source_key: row.get("source_key"),
                source_conversation: row.get("source_conversation"),
                conversation_id: row.get("conversation_id"),
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }
    /// Spend a transport attempt only after preparation, immediately before
    /// executing HTTP. The atomic update rechecks eligibility and proves that
    /// this delivery still owns the unexpired, persisted claim.
    pub async fn begin_push_attempt(
        &self,
        delivery: PushDelivery,
    ) -> Result<Option<PushAttempt>, sqlx::Error> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            r#"UPDATE push_outbox AS o
               SET attempts=attempts+1,
                   next_attempt=unixepoch()+MIN(3600,60*(1<<(attempts+1)))
               WHERE o.id=? AND o.state='pending' AND o.attempts=? AND o.attempts<5
                 AND o.next_attempt=? AND o.next_attempt>unixepoch()
                 AND o.created_at>=unixepoch()-86400
                 AND o.registration_id=?
                 AND NOT EXISTS(SELECT 1 FROM project_conversations c WHERE c.conversation_id=o.conversation_id AND c.notification_level='mute')
                 AND EXISTS(
                   SELECT 1 FROM push_registrations r
                   JOIN devices d ON d.id=r.device_id
                   JOIN push_intents i ON i.id=o.intent_id
                   WHERE r.id=o.registration_id AND r.revoked=0 AND d.revoked_at IS NULL
                     AND r.endpoint=? AND r.sender_secret=? AND r.preview_key IS ?
                     AND i.source_key=? AND i.conversation_id=?
                     AND NOT EXISTS(SELECT 1 FROM push_presence p WHERE p.device_id=d.id
                       AND p.foreground_until_ms>unixepoch('subsec')*1000
                       AND (p.conversation_id IS NULL OR p.conversation_id=o.conversation_id))
                     AND NOT EXISTS(SELECT 1 FROM push_suppressed s
                       WHERE s.intent_id=i.id AND s.device_id=d.id)
                     AND (i.source_key NOT LIKE 'approval:%' OR EXISTS(
                       SELECT 1 FROM approvals a WHERE a.server_request_id=substr(i.source_key,10)
                         AND a.state='pending'))
                     AND (i.source_key NOT LIKE 'question:%' OR EXISTS(
                       SELECT 1 FROM async_questions q WHERE q.id=substr(i.source_key,10)
                         AND q.conversation_id=i.conversation_id AND q.state='pending'
                         AND q.expires_at_ms>unixepoch('subsec')*1000))
                     AND NOT EXISTS(SELECT 1 FROM events e WHERE e.event_id=i.source_key
                       AND json_extract(e.payload_json,'$.event.type')='approval_opened'
                       AND NOT EXISTS(SELECT 1 FROM approvals a
                         WHERE a.server_request_id=json_extract(e.payload_json,'$.event.data.request_id')
                           AND a.thread_id=json_extract(e.payload_json,'$.threadId')
                           AND a.turn_id=json_extract(e.payload_json,'$.turnId') AND a.state='pending')))
               RETURNING attempts,next_attempt"#,
        )
        .bind(&delivery.id)
        .bind(delivery.attempts)
        .bind(delivery.claim_until)
        .bind(&delivery.registration_id)
        .bind(&delivery.endpoint)
        .bind(&delivery.sender_secret)
        .bind(&delivery.preview_key)
        .bind(&delivery.source_key)
        .bind(&delivery.source_conversation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(attempts, claim_until)| PushAttempt {
            attempts,
            id: delivery.id,
            claim_until,
            registration_id: delivery.registration_id,
            endpoint: delivery.endpoint,
            sender_secret: delivery.sender_secret,
        }))
    }

    /// The Worker no longer recognizes this capability. Retire it and cancel
    /// its queued deliveries atomically, preserving other devices and any
    /// replacement capability installed while HTTP was in flight. Normal
    /// revocation cleanup can remove the retired record afterwards.
    pub async fn retire_push_registration(&self, attempt: &PushAttempt) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let retired = sqlx::query("UPDATE push_registrations SET revoked=1 WHERE id=? AND endpoint=? AND sender_secret=? AND revoked=0")
            .bind(&attempt.registration_id)
            .bind(&attempt.endpoint)
            .bind(&attempt.sender_secret)
            .execute(&mut *tx)
            .await?;
        if retired.rows_affected() == 1 {
            sqlx::query("UPDATE push_outbox SET state=CASE WHEN id=? THEN 'failed' ELSE 'cancelled' END WHERE registration_id=? AND state='pending'")
                .bind(&attempt.id)
                .bind(&attempt.registration_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Preparation failed or content disappeared. Keep backoff without spending
    /// an attempt, and never let an expired worker finish a replacement claim.
    pub async fn finish_push_claim(
        &self,
        delivery: &PushDelivery,
        state: &str,
    ) -> Result<(), sqlx::Error> {
        self.finish_owned_push(&delivery.id, delivery.attempts, delivery.claim_until, state)
            .await
    }

    pub async fn finish_push_attempt(
        &self,
        attempt: &PushAttempt,
        state: &str,
    ) -> Result<(), sqlx::Error> {
        self.finish_owned_push(&attempt.id, attempt.attempts, attempt.claim_until, state)
            .await
    }

    async fn finish_owned_push(
        &self,
        id: &str,
        attempts: i64,
        claim_until: i64,
        state: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE push_outbox SET state=?,next_attempt=unixepoch()+MIN(3600,60*(1<<attempts)) WHERE id=? AND state='pending' AND attempts=? AND next_attempt=? AND next_attempt>unixepoch()")
            .bind(state).bind(id).bind(attempts).bind(claim_until)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn next_push_revocation(&self) -> Result<Option<PushRevocation>, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row=sqlx::query("SELECT * FROM push_registrations WHERE revoked=1 AND revoke_attempts<5 AND next_revoke<=unixepoch() LIMIT 1").fetch_optional(&mut *tx).await?;
        let value = if let Some(row) = row {
            let id: String = row.get("id");
            sqlx::query("UPDATE push_registrations SET revoke_attempts=revoke_attempts+1,next_revoke=unixepoch()+3600 WHERE id=?").bind(&id).execute(&mut *tx).await?;
            Some(PushRevocation {
                id,
                endpoint: row.get("endpoint"),
                sender_secret: row.get("sender_secret"),
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(value)
    }
    pub async fn finish_push_revocation(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM push_registrations WHERE id=? AND revoked=1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn store() -> Store {
        store_at("sqlite::memory:").await
    }
    async fn store_at(url: &str) -> Store {
        let store = Store::connect(url).await.unwrap();
        for id in ["parent", "other"] {
            seed_project_thread(&store, id, "Project", "/tmp", None, None, "now")
                .await
                .unwrap();
        }
        for id in ["phone-a", "phone-b"] {
            store
                .upsert_owner_device(id, id, "{}", "now")
                .await
                .unwrap();
        }
        store
            .register_push("phone-a", "a", "https://push.test", "secret-a")
            .await
            .unwrap();
        store
            .register_push("phone-b", "b", "https://push.test", "secret-b")
            .await
            .unwrap();
        store
    }
    fn event() -> HostEventEnvelope {
        HostEventEnvelope {
            event_id: uuid::Uuid::new_v4().to_string(),
            host_epoch: "epoch".into(),
            sequence: 1,
            occurred_at: "now".into(),
            request_id: None,
            device_id: None,
            conversation_id: Some("parent".into()),
            message_id: None,
            thread_id: None,
            turn_id: None,
            item_id: None,
            approval_id: None,
            event: WonderEvent::MessageState {
                state: wonder_api::DeliveryState::Completed,
            },
        }
    }
    #[tokio::test]
    async fn push_intent_is_atomic_and_routes_are_device_scoped() {
        let store = store().await;
        let event = event();
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO events VALUES(?,?,?,?,?)")
            .bind(&event.event_id)
            .bind(&event.host_epoch)
            .bind(1)
            .bind("now")
            .bind(serde_json::to_string(&event).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM push_intents")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            0
        );
        store.append_event(&event).await.unwrap();
        store.append_event(&event).await.unwrap();
        store.distribute_push().await.unwrap();
        store.distribute_push().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM push_outbox")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            2
        );
        let delivery = store.next_push().await.unwrap().unwrap();
        let owner = if delivery.registration_id == "a" {
            "phone-a"
        } else {
            "phone-b"
        };
        let other = if owner == "phone-a" {
            "phone-b"
        } else {
            "phone-a"
        };
        assert_eq!(
            store
                .push_route(owner, &delivery.route_id)
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        assert!(store
            .push_route(other, &delivery.route_id)
            .await
            .unwrap()
            .is_none());
        store.revoke_owner_device(owner, "later").await.unwrap();
        assert!(store
            .push_route(owner, &delivery.route_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .register_push(owner, "new", "https://push.test", "secret")
            .await
            .is_err());
        assert_eq!(
            store.next_push_revocation().await.unwrap().unwrap().id,
            delivery.registration_id
        );
    }
    // One matrix owns the persisted event-to-delivery policy; requests stay in-app.
    #[tokio::test]
    async fn chat_notification_levels_filter_finished_failed_question_and_approval() {
        use ChatNotificationLevel::{Default, Follow, Mute};
        for level in [Default, Follow, Mute] {
            let store = store().await;
            store.revoke_push("phone-b").await.unwrap();
            bind_project_fixture(&store, "parent", AgentFamily::Codex, "thread", None, "now")
                .await
                .unwrap();
            let MessageInsert::Inserted(message) = store
                .insert_message("phone-a", "receipt", "hello", "hash", "parent", "now")
                .await
                .unwrap()
            else {
                panic!()
            };
            store
                .update_message_delivery(
                    &message.id,
                    "accepted_by_codex",
                    Some("thread"),
                    Some("turn"),
                )
                .await
                .unwrap();
            store
                .set_chat_notification_level("parent", level)
                .await
                .unwrap();
            for (index, state) in [
                wonder_api::DeliveryState::Completed,
                wonder_api::DeliveryState::Failed,
            ]
            .into_iter()
            .enumerate()
            {
                let mut event = event();
                event.sequence = index as u64 + 1;
                event.event = WonderEvent::MessageState { state };
                store.append_event(&event).await.unwrap();
            }
            store
                .save_async_question("parent", "thread", "turn", "ask", "[]", i64::MAX)
                .await
                .unwrap();
            insert_approval(&store, "approval", "thread", "turn", "").await;
            store.distribute_push().await.unwrap();
            let kinds: Vec<String> = sqlx::query_scalar(
                "SELECT kind FROM push_outbox WHERE state='pending' ORDER BY rowid",
            )
            .fetch_all(&store.pool)
            .await
            .unwrap();
            assert_eq!(
                kinds,
                if level == Mute {
                    vec![]
                } else {
                    vec!["completed", "attention", "attention", "attention"]
                },
                "{level:?}"
            );
            assert_eq!(
                store.list_pending_approvals().await.unwrap().len(),
                1,
                "{level:?}: in-app approval"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM async_questions WHERE state='pending'"
                )
                .fetch_one(&store.pool)
                .await
                .unwrap(),
                1
            );
            // Follow also adds completions from verified child tasks.
            store
                .register_subagent_ownership(
                    "child",
                    "parent",
                    "child-thread",
                    "thread",
                    Some("Review"),
                    None,
                    None,
                    "{}",
                    Some("runtime"),
                    Some(false),
                    "active",
                    Some(false),
                    "now",
                )
                .await
                .unwrap();
            let mut child = event();
            child.sequence = 3;
            child.conversation_id = Some("child".into());
            store.append_event(&child).await.unwrap();
            store.distribute_push().await.unwrap();
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM push_outbox WHERE state='pending'")
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(
                count,
                match level {
                    Default => 4,
                    Follow => 5,
                    Mute => 0,
                }
            );
        }
    }

    #[tokio::test]
    async fn turn_outcomes_choose_the_push_kind() {
        use wonder_api::DeliveryState::*;
        let store = store().await;
        for (index, (state, turn, kind)) in [
            (Completed, Some("turn"), Some("completed")),
            (Failed, Some("turn"), Some("attention")),
            (Interrupted, Some("turn"), None),
            // A turn that ended with an outcome Wonder cannot read.
            (Uncertain, Some("turn"), Some("attention")),
            // A send whose delivery is unknown has no turn and stays quiet.
            (Uncertain, None, None),
        ]
        .into_iter()
        .enumerate()
        {
            let mut event = event();
            event.sequence = index as u64 + 1;
            event.turn_id = turn.map(str::to_owned);
            event.event = WonderEvent::MessageState { state };
            store.append_event(&event).await.unwrap();
            let saved: Option<String> =
                sqlx::query_scalar("SELECT kind FROM push_intents WHERE source_key=?")
                    .bind(&event.event_id)
                    .fetch_optional(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(saved.as_deref(), kind, "{event:?}");
        }
    }
    #[tokio::test]
    async fn push_rotation_keeps_routes_and_revokes_old_capability() {
        let store = store().await;
        store.append_event(&event()).await.unwrap();
        store.distribute_push().await.unwrap();
        let route: String =
            sqlx::query_scalar("SELECT route_id FROM push_outbox WHERE registration_id='a'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        store
            .register_push("phone-a", "new-a", "https://push.test", "new-secret")
            .await
            .unwrap();
        assert_eq!(store.next_push_revocation().await.unwrap().unwrap().id, "a");
        store.finish_push_revocation("a").await.unwrap();
        assert_eq!(
            store
                .push_route("phone-a", &route)
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        assert!(store
            .register_push("phone-b", "new-a", "https://push.test", "hijack")
            .await
            .is_err());
    }
    #[tokio::test]
    async fn revoked_registration_cannot_be_reactivated_or_replace_current_registration() {
        let store = store().await;
        store.revoke_push("phone-a").await.unwrap();
        store
            .register_push("phone-a", "new", "https://push.test", "new-secret")
            .await
            .unwrap();
        assert!(store
            .register_push("phone-a", "a", "https://push.test", "secret-a")
            .await
            .is_err());
        let active: String = sqlx::query_scalar(
            "SELECT id FROM push_registrations WHERE device_id='phone-a' AND revoked=0",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(active, "new");
    }

    #[tokio::test]
    async fn push_registration_retirement_persists_and_cancels_only_its_deliveries() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("push.db").display());
        let store = store_at(&url).await;
        store.append_event(&event()).await.unwrap();
        let mut later = event();
        later.sequence = 2;
        store.append_event(&later).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let registration = delivery.registration_id.clone();
        let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
        store.retire_push_registration(&attempt).await.unwrap();
        store.retire_push_registration(&attempt).await.unwrap();
        let outcomes: Vec<(i64, String)> = sqlx::query_as(
            "SELECT attempts,state FROM push_outbox WHERE registration_id=? ORDER BY attempts DESC",
        )
        .bind(&registration)
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            outcomes,
            vec![(1, "failed".into()), (0, "cancelled".into())]
        );
        let unaffected: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM push_outbox WHERE registration_id!=? AND state='pending' AND attempts=0",
        )
        .bind(&registration)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(unaffected, 2);
        // A stale transient completion cannot resurrect the retired delivery.
        store
            .finish_push_attempt(&attempt, "pending")
            .await
            .unwrap();
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        let retired: bool = sqlx::query_scalar("SELECT revoked FROM push_registrations WHERE id=?")
            .bind(&registration)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(retired);
        assert_ne!(
            store.next_push().await.unwrap().unwrap().registration_id,
            registration
        );
        later = event();
        later.sequence = 3;
        store.append_event(&later).await.unwrap();
        store.distribute_push().await.unwrap();
        let retired_pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM push_outbox WHERE registration_id=? AND state='pending'",
        )
        .bind(&registration)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(retired_pending, 0);
        let device = if registration == "a" {
            "phone-a"
        } else {
            "phone-b"
        };
        assert!(store
            .register_push(device, &registration, "https://push.test", "old-secret")
            .await
            .is_err());
        store
            .register_push(device, "replacement", "https://push.test", "new-secret")
            .await
            .unwrap();
        let active: String =
            sqlx::query_scalar("SELECT id FROM push_registrations WHERE device_id=? AND revoked=0")
                .bind(device)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(active, "replacement");
    }

    #[tokio::test]
    async fn push_registration_retirement_cannot_disable_a_replacement_capability() {
        for changed in ["id", "endpoint", "secret"] {
            let store = store().await;
            store.revoke_push("phone-b").await.unwrap();
            store.append_event(&event()).await.unwrap();
            store.distribute_push().await.unwrap();
            let delivery = store.next_push().await.unwrap().unwrap();
            let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
            let id = if changed == "id" { "new-a" } else { "a" };
            let endpoint = if changed == "endpoint" {
                "https://new.push.test"
            } else {
                "https://push.test"
            };
            let secret = if changed == "secret" {
                "new-secret"
            } else {
                "secret-a"
            };
            store
                .register_push("phone-a", id, endpoint, secret)
                .await
                .unwrap();
            // This HTTP response belongs to the previous capability.
            store.retire_push_registration(&attempt).await.unwrap();
            let active: (String, String, String) = sqlx::query_as(
                "SELECT id,endpoint,sender_secret FROM push_registrations WHERE device_id='phone-a' AND revoked=0",
            )
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(
                active,
                (id.into(), endpoint.into(), secret.into()),
                "{changed}"
            );
        }
    }

    #[tokio::test]
    async fn push_claims_have_a_lease_and_bounded_retries() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        store.append_event(&event()).await.unwrap();
        store.distribute_push().await.unwrap();
        for expected in 1..=5 {
            let delivery = store.next_push().await.unwrap().unwrap();
            assert_eq!(delivery.attempts, expected - 1);
            assert!(store.next_push().await.unwrap().is_none());
            let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
            assert_eq!(attempt.attempts, expected);
            let stored: (i64, i64) = sqlx::query_as(
                "SELECT attempts,next_attempt-unixepoch() FROM push_outbox WHERE id=?",
            )
            .bind(&attempt.id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(stored.0, expected);
            assert!((60 * (1 << expected) - 1..=60 * (1 << expected)).contains(&stored.1));
            assert!(store.next_push().await.unwrap().is_none());
            store
                .finish_push_attempt(&attempt, "pending")
                .await
                .unwrap();
            sqlx::query("UPDATE push_outbox SET next_attempt=0")
                .execute(&store.pool)
                .await
                .unwrap();
        }
        assert!(store.next_push().await.unwrap().is_none());
        let stored: (i64, String) = sqlx::query_as("SELECT attempts,state FROM push_outbox")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(stored, (5, "failed".into()));
        eprintln!(
            "admission limit: stored attempts={} state={}",
            stored.0, stored.1
        );
    }

    #[tokio::test]
    async fn push_claims_recover_across_restart_without_reusing_stale_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("push.db").display());
        let store = store_at(&url).await;
        store.revoke_push("phone-b").await.unwrap();
        store.append_event(&event()).await.unwrap();
        store.distribute_push().await.unwrap();
        let mut stale = store.next_push().await.unwrap().unwrap();
        assert_eq!(stale.attempts, 0);
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        assert!(
            store.next_push().await.unwrap().is_none(),
            "restart preserves the lease"
        );
        // Advance the persisted and observed deadline together to model elapsed
        // time without sleeping a minute. The replacement gets a later token.
        stale.claim_until = sqlx::query_scalar(
            "UPDATE push_outbox SET next_attempt=unixepoch()-1 RETURNING next_attempt",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let (first, second) = tokio::join!(store.next_push(), store.next_push());
        let candidates: Vec<_> = [first.unwrap(), second.unwrap()]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(
            candidates.len(),
            1,
            "only one concurrent worker owns the replacement"
        );
        let replacement = candidates.into_iter().next().unwrap();
        assert_eq!(replacement.attempts, 0);
        store.finish_push_claim(&stale, "cancelled").await.unwrap();
        assert!(store.begin_push_attempt(stale).await.unwrap().is_none());
        let attempt = store
            .begin_push_attempt(replacement)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attempt.attempts, 1);
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        assert!(
            store.next_push().await.unwrap().is_none(),
            "an interrupted send retains backoff"
        );
        let stored: (i64, String, i64) =
            sqlx::query_as("SELECT attempts,state,next_attempt-unixepoch() FROM push_outbox")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!((stored.0, stored.1.as_str()), (1, "pending"));
        assert!((1..=120).contains(&stored.2));
        sqlx::query("UPDATE push_outbox SET next_attempt=unixepoch()-1")
            .execute(&store.pool)
            .await
            .unwrap();
        let replacement = store.next_push().await.unwrap().unwrap();
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        let attempt = store
            .begin_push_attempt(replacement)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attempt.attempts, 2);
        eprintln!("restart: unsent claim attempts=0; interrupted admitted send attempts=1 state=pending backoff={}s; recovered admission=2; concurrent claims=1; stale owners rejected", stored.2);
    }

    #[tokio::test]
    async fn push_admission_rechecks_claimed_registration_intent_and_row() {
        for case in [
            "registration",
            "device",
            "rotation",
            "credentials",
            "withdrawn",
            "expired",
            "foreground",
            "lease",
            "mute",
        ] {
            let store = store().await;
            store.revoke_push("phone-b").await.unwrap();
            store
                .enqueue_external_push(
                    "codex:admission:completed",
                    "parent",
                    "completed",
                    Some("Private reply"),
                )
                .await
                .unwrap();
            store.distribute_push().await.unwrap();
            let delivery = store.next_push().await.unwrap().unwrap();
            assert!(store.push_preview(&delivery).await.unwrap().is_some());
            match case {
                "mute" => {
                    store
                        .set_chat_notification_level("parent", ChatNotificationLevel::Mute)
                        .await
                        .unwrap();
                    // Unmuting cannot revive an already cancelled delivery.
                    store
                        .set_chat_notification_level("parent", ChatNotificationLevel::Follow)
                        .await
                        .unwrap();
                }
                "registration" => store.revoke_push("phone-a").await.unwrap(),
                "device" => {
                    assert!(store.revoke_owner_device("phone-a", "later").await.unwrap());
                }
                "rotation" => store
                    .register_push("phone-a", "new", "https://push.test", "new-secret")
                    .await
                    .unwrap(),
                "credentials" => store
                    .register_push("phone-a", "a", "https://push.test", "new-secret")
                    .await
                    .unwrap(),
                "withdrawn" => store
                    .withdraw_external_push("codex:admission:completed")
                    .await
                    .unwrap(),
                "expired" => {
                    sqlx::query("UPDATE push_outbox SET created_at=unixepoch()-86401")
                        .execute(&store.pool)
                        .await
                        .unwrap();
                }
                "foreground" => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;
                    store
                        .push_presence("phone-a", true, Some("parent"), now, now)
                        .await
                        .unwrap();
                    // Leaving the conversation must not revive its unsent push.
                    store
                        .push_presence("phone-a", false, None, now + 1, now + 1)
                        .await
                        .unwrap();
                }
                "lease" => {
                    sqlx::query("UPDATE push_outbox SET next_attempt=unixepoch()-1")
                        .execute(&store.pool)
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let id = delivery.id.clone();
            assert!(
                store.begin_push_attempt(delivery).await.unwrap().is_none(),
                "{case}"
            );
            let stored: (i64, String) =
                sqlx::query_as("SELECT attempts,state FROM push_outbox WHERE id=?")
                    .bind(id)
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(stored.0, 0, "{case}");
            let expected = match case {
                "registration" | "device" | "rotation" | "withdrawn" | "mute" => "cancelled",
                "foreground" => "suppressed",
                _ => "pending",
            };
            assert_eq!(stored.1, expected, "{case}");
            eprintln!(
                "post-claim {case}: attempts={} state={} admitted=false",
                stored.0, stored.1
            );
        }
    }

    #[tokio::test]
    async fn foreground_arrivals_stay_suppressed_after_background_and_other_devices_receive() {
        let store = store().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        store
            .push_presence("phone-a", true, None, now, now)
            .await
            .unwrap();
        store.append_event(&event()).await.unwrap();
        store
            .push_presence("phone-a", false, None, now + 1, now + 1)
            .await
            .unwrap();
        // A delayed heartbeat must not undo the newer background transition.
        store
            .push_presence("phone-a", true, None, now, now + 2)
            .await
            .unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(delivery.registration_id, "b");
        assert!(store.next_push().await.unwrap().is_none());
        let mut next = event();
        next.sequence = 2;
        store.append_event(&next).await.unwrap();
        store.distribute_push().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM push_outbox WHERE registration_id='a'"
            )
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn only_the_conversation_open_on_screen_stays_quiet() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let completed = |sequence: u64, conversation: &str| {
            let mut value = event();
            value.sequence = sequence;
            value.conversation_id = Some(conversation.into());
            value
        };
        let sent = |store: &Store| {
            let pool = store.pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT conversation_id FROM push_outbox WHERE state='pending' ORDER BY rowid",
                )
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        // Reading "parent": its reply stays quiet, another chat's does not.
        store
            .push_presence("phone-a", true, Some("parent"), now, now)
            .await
            .unwrap();
        store.append_event(&completed(1, "parent")).await.unwrap();
        store.append_event(&completed(2, "other")).await.unwrap();
        store.distribute_push().await.unwrap();
        assert_eq!(sent(&store).await, vec!["other".to_owned()]);
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(delivery.conversation_id, "other");
        assert_eq!(delivery.attempts, 0);
        // Opening "other" after the claim but before admission keeps it quiet too.
        store
            .push_presence("phone-a", true, Some("other"), now + 1, now + 1)
            .await
            .unwrap();
        assert!(store.begin_push_attempt(delivery).await.unwrap().is_none());
        let counters: Vec<i64> = sqlx::query_scalar("SELECT attempts FROM push_outbox")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert!(counters.iter().all(|attempts| *attempts == 0));
        // On the chat list ("") nothing is quiet.
        store
            .push_presence("phone-a", true, Some(""), now + 2, now + 2)
            .await
            .unwrap();
        store.append_event(&completed(3, "parent")).await.unwrap();
        store.distribute_push().await.unwrap();
        assert_eq!(sent(&store).await, vec!["parent".to_owned()]);
        let delivery = store.next_push().await.unwrap().unwrap();
        let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
        assert_eq!(attempt.attempts, 1);
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        // An older phone (no conversation) keeps everything quiet while in
        // front, as before, including what arrives after it leaves.
        store
            .push_presence("phone-a", true, None, now + 3, now + 3)
            .await
            .unwrap();
        assert!(sent(&store).await.is_empty());
        store.append_event(&completed(4, "other")).await.unwrap();
        store
            .push_presence("phone-a", false, None, now + 4, now + 4)
            .await
            .unwrap();
        store.distribute_push().await.unwrap();
        assert!(sent(&store).await.is_empty());
    }

    #[tokio::test]
    async fn foreground_cancels_inflight_claims_and_crashed_presence_expires() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        store.append_event(&event()).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(delivery.attempts, 0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        store
            .push_presence("phone-a", true, None, now, now)
            .await
            .unwrap();
        assert!(store.begin_push_attempt(delivery).await.unwrap().is_none());
        let counters: Vec<i64> = sqlx::query_scalar("SELECT attempts FROM push_outbox")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert!(counters.iter().all(|attempts| *attempts == 0));
        sqlx::query("UPDATE push_presence SET foreground_until_ms=0")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut next = event();
        next.sequence = 2;
        store.append_event(&next).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
        assert_eq!(attempt.attempts, 1);
    }

    #[tokio::test]
    async fn previews_use_exact_turn_and_questions_expire() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        store
            .complete_assistant_message(
                "parent",
                "thread",
                "one",
                "item-one",
                "Correct answer",
                "1",
            )
            .await
            .unwrap();
        store
            .complete_assistant_message(
                "parent",
                "thread",
                "two",
                "item-two",
                "Unrelated later answer",
                "2",
            )
            .await
            .unwrap();
        let mut event = event();
        event.thread_id = Some("thread".into());
        event.turn_id = Some("one".into());
        store.append_event(&event).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(
            store.push_preview(&delivery).await.unwrap().unwrap().body,
            "Correct answer"
        );
        let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        store
            .save_async_question(
                "parent",
                "thread",
                "two",
                "ask",
                r#"[{"title":"Which day?"}]"#,
                i64::MAX,
            )
            .await
            .unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(
            store.push_preview(&delivery).await.unwrap().unwrap().body,
            "Which day?"
        );
        sqlx::query("UPDATE async_questions SET state='answered'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.push_preview(&delivery).await.unwrap().is_none());
        assert!(store.begin_push_attempt(delivery).await.unwrap().is_none());
    }

    /// A live item event for `item` in `turn`, as wonderd publishes it.
    fn item_event(sequence: u64, turn: &str, item: &str, item_type: &str) -> HostEventEnvelope {
        HostEventEnvelope {
            sequence,
            thread_id: Some("thread".into()),
            turn_id: Some(turn.into()),
            event: WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(
                    serde_json::json!({"turnId": turn, "itemId": item, "itemType": item_type, "state": "completed", "lifecycleAuthority": 2})
                        .to_string(),
                ),
            },
            ..event()
        }
    }

    async fn completed_body(store: &Store, sequence: u64, turn: &str) -> String {
        let event = HostEventEnvelope {
            sequence,
            thread_id: Some("thread".into()),
            turn_id: Some(turn.into()),
            ..event()
        };
        store.append_event(&event).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let preview = store.push_preview(&delivery).await.unwrap().unwrap();
        // The phone clears this notification once it has read this far.
        let read = preview
            .read_through
            .expect("a finished reply has a history position");
        assert_eq!(
            (read.host_epoch.as_str(), read.sequence),
            (event.host_epoch.as_str(), sequence)
        );
        let body = preview.body;
        let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        body
    }

    // A reply's final text is its notification; a reply that ended on a
    // tool call says only that it finished, never the commentary before it.
    #[tokio::test]
    async fn completion_quotes_the_final_reply_not_commentary() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        let mut sequence = 0;
        for (turn, steps, body) in [
            (
                "ended-on-text",
                [
                    ("commandExecution", ""),
                    ("agentMessage", "All tests pass."),
                ],
                "All tests pass.",
            ),
            (
                "ended-on-a-call",
                [
                    ("agentMessage", "Running the tests now."),
                    ("commandExecution", ""),
                ],
                "A reply finished.",
            ),
            (
                "ended-on-markdown",
                [
                    ("commandExecution", ""),
                    (
                        "agentMessage",
                        "## Summary\n\n**All tests pass** in `wonder-store`; see [the log](https://example.com/log).\n\n- Fixed *two* bugs\n- Kept snake_case_names and 2 * 3 * 4\n\n```sh\ncargo test -p wonder-store\n```",
                    ),
                ],
                "Summary\n\nAll tests pass in wonder-store; see the log.\n\n• Fixed two bugs\n• Kept snake_case_names and 2 * 3 * 4\n\ncargo test -p wonder-store",
            ),
        ] {
            for (index, (item_type, text)) in steps.into_iter().enumerate() {
                let item = format!("{turn}-{index}");
                if !text.is_empty() {
                    store
                        .complete_assistant_message(
                            "parent",
                            "thread",
                            turn,
                            &item,
                            text,
                            &index.to_string(),
                        )
                        .await
                        .unwrap();
                }
                // Started, then completed: the step's place is where it started.
                for _ in 0..2 {
                    sequence += 1;
                    store
                        .append_event(&item_event(sequence, turn, &item, item_type))
                        .await
                        .unwrap();
                }
            }
            sequence += 1;
            assert_eq!(completed_body(&store, sequence, turn).await, body, "{turn}");
        }
        // Work on the computer without closing text says only that it
        // finished; its closing text reads as plain text like any reply.
        store.start_event_epoch("live").await.unwrap();
        store
            .commit_event(&mut HostEventEnvelope {
                host_epoch: "live".into(),
                event: WonderEvent::Activity {
                    category: "native_status".into(),
                    state: "changed".into(),
                    detail: None,
                },
                ..event()
            })
            .await
            .unwrap();
        for (prompt, text, body) in [
            ("p1", None, "Claude finished replying on your computer."),
            (
                "p2",
                Some("# Done\n\n> **Shipped** the `fix`."),
                "Done\n\nShipped the fix.",
            ),
        ] {
            store
                .enqueue_external_push(
                    &format!("claude:s1:{prompt}:completed"),
                    "parent",
                    "completed",
                    text,
                )
                .await
                .unwrap();
            store.distribute_push().await.unwrap();
            let delivery = store.next_push().await.unwrap().unwrap();
            let preview = store.push_preview(&delivery).await.unwrap().unwrap();
            assert_eq!(preview.body, body, "{prompt}");
            // The agent's own history has no Cupola event: the reply counts
            // as read once the phone has seen everything recorded so far.
            let head: (String, i64) =
                sqlx::query_as("SELECT host_epoch,last_sequence FROM sync_state WHERE singleton=1")
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            let read = preview
                .read_through
                .expect("an outside reply has a history position");
            assert!(head.1 > 0, "{prompt}");
            assert_eq!((read.host_epoch, read.sequence as i64), head, "{prompt}");
            let attempt = store.begin_push_attempt(delivery).await.unwrap().unwrap();
            store
                .finish_push_attempt(&attempt, "delivered")
                .await
                .unwrap();
        }
    }

    // Formatting removes only well-formed markup: words, code contents and
    // stray symbols survive, and a reply made only of markup keeps its text.
    #[test]
    fn plain_reply_keeps_words_and_literal_symbols() {
        for (markdown, plain) in [
            (
                "Use **bold**, __strong__, *it*, _em_ and ~~old~~.",
                "Use bold, strong, it, em and old.",
            ),
            ("A ``code with ` tick`` span", "A code with ` tick span"),
            (
                "Escaped \\*stars\\* and \\# hash",
                "Escaped *stars* and # hash",
            ),
            (
                "![chart](a.png) and <https://example.com>",
                "chart and https://example.com",
            ),
            (
                "Unclosed **bold and `tick and [link](",
                "Unclosed **bold and `tick and [link](",
            ),
            // Literal text is never rewritten: indented code, a heading's
            // own # and a paragraph's indented continuation.
            ("## Learn C#\n### Done ###\n# F# #", "Learn C#\nDone\nF#"),
            (
                "Run this:\n\n    cargo test **all**\n\tmake *x*\n\nThen _check_.",
                "Run this:\n\ncargo test **all**\nmake *x*\n\nThen check.",
            ),
            (
                "A **long**\n    *continued* line\n- item\n\n    more **text**",
                "A long\ncontinued line\n• item\n\nmore text",
            ),
            (
                "1. First\n2. Second\n* * *\n+ Third",
                "1. First\n2. Second\n• Third",
            ),
            (
                "| Name | Result |\n|---|:-:|\n| `a` | **ok** |",
                "Name · Result\na · ok",
            ),
            ("~~~\n# not a heading\n~~~", "# not a heading"),
            ("Price is 5*3 and a_b_c", "Price is 5*3 and a_b_c"),
            ("---", "---"),
        ] {
            assert_eq!(plain_reply(markdown), plain, "{markdown}");
        }
        // Bounded: very long input does constant-bounded work and stays text.
        assert_eq!(plain_reply(&"*".repeat(100_000)), "*".repeat(4000));
    }

    async fn insert_approval(store: &Store, request: &str, thread: &str, turn: &str, extra: &str) {
        let params = format!(
            r#"{{"threadId":"{thread}","turnId":"{turn}","reason":"Install dependencies?","command":"npm ci"{extra}}}"#
        );
        store
            .insert_pending_approval(
                request,
                "item/commandExecution/requestApproval",
                &params,
                "now",
            )
            .await
            .unwrap();
    }
    async fn outbox(store: &Store) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT o.registration_id,o.conversation_id,i.source_key FROM push_outbox o JOIN push_intents i ON i.id=o.intent_id ORDER BY o.created_at,o.rowid")
            .fetch_all(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn approval_preview_is_exact_and_stale_requests_are_cancelled() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        bind_project_fixture(&store, "parent", AgentFamily::Codex, "thread", None, "now")
            .await
            .unwrap();
        assert!(store
            .approval_push_conversation("thread", "turn")
            .await
            .unwrap()
            .is_none());
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message(
                "phone-a",
                "early-client",
                "hello",
                "hash",
                "parent",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission(&message.id, "thread")
            .await
            .unwrap();
        insert_approval(
            &store,
            "request",
            "thread",
            "turn",
            r#","cwd":"/workspace/project","kind":"command","supportsDenyMessage":true"#,
        )
        .await;
        // The runtime's approval_opened event no longer adds a second intent.
        let mut event = event();
        event.thread_id = Some("thread".into());
        event.turn_id = Some("turn".into());
        event.event = WonderEvent::ApprovalOpened {
            request_id: "request".into(),
        };
        store.append_event(&event).await.unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        assert_eq!(delivery.conversation_id, "parent");
        let preview = store.push_preview(&delivery).await.unwrap().unwrap();
        assert!(
            serde_json::to_value(&preview).unwrap()["approval"].is_object(),
            "a complete command must carry its exact encrypted action reference"
        );
        assert_eq!(
            preview.body,
            "Install dependencies?\nnpm ci\n/workspace/project"
        );
        assert!(preview.title.ends_with("· Approval requested"));
        assert!(preview.approval.unwrap().supports_deny_message);
        assert_eq!(outbox(&store).await.len(), 1);
        sqlx::query("UPDATE approvals SET state='resolved'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.push_preview(&delivery).await.unwrap().is_none());
        assert!(store.begin_push_attempt(delivery).await.unwrap().is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM push_outbox")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn every_pending_approval_notifies_once_even_before_its_turn_is_mapped() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        // Arrives before the dispatch response maps its turn or thread.
        insert_approval(&store, "early", "thread", "turn", "").await;
        // Resolved on the Mac before the worker ran.
        insert_approval(&store, "answered", "thread", "turn", "").await;
        sqlx::query("UPDATE approvals SET state='resolved' WHERE server_request_id='answered'")
            .execute(&store.pool)
            .await
            .unwrap();
        // Never routable: dropped after the grace period.
        insert_approval(&store, "orphan", "elsewhere", "", "").await;
        store.distribute_push().await.unwrap();
        assert!(outbox(&store).await.is_empty(), "nothing is sent unrouted");
        bind_project_fixture(&store, "parent", AgentFamily::Codex, "thread", None, "now")
            .await
            .unwrap();
        assert!(store
            .approval_push_conversation("thread", "turn")
            .await
            .unwrap()
            .is_none());
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message(
                "phone-a",
                "early-client",
                "hello",
                "hash",
                "parent",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission(&message.id, "thread")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE push_intents SET created_at=created_at-1000 WHERE source_key='approval:orphan'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store.distribute_push().await.unwrap();
        store.distribute_push().await.unwrap();
        assert_eq!(
            outbox(&store).await,
            vec![("a".into(), "parent".into(), "approval:early".into())]
        );
        let waiting: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM push_intents WHERE distributed=0")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(waiting, 0);
        // The Permission list routes the same request to the same chat.
        assert_eq!(
            store
                .approval_push_conversation("thread", "turn")
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        assert_eq!(
            store
                .approval_push_conversation("thread", "")
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        store
            .register_subagent_ownership(
                "child",
                "parent",
                "child-thread",
                "thread",
                Some("Review"),
                None,
                None,
                "{}",
                Some("runtime"),
                Some(false),
                "active",
                Some(false),
                "now",
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .approval_conversation_id("child-thread", "child-turn")
                .await
                .unwrap()
                .as_deref(),
            Some("child")
        );
        assert_eq!(
            store
                .approval_push_conversation("child-thread", "child-turn")
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        assert_eq!(
            store
                .approval_push_conversation("child-thread", "")
                .await
                .unwrap()
                .as_deref(),
            Some("parent")
        );
        sqlx::query(
            "UPDATE runtime_bindings SET provider_store='foreign' WHERE conversation_id='child'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(store
            .approval_push_conversation("child-thread", "child-turn")
            .await
            .unwrap()
            .is_none());

        sqlx::query("UPDATE runtime_bindings SET provider_store='another-store' WHERE conversation_id='parent'").execute(&store.pool).await.unwrap();
        assert!(store
            .approval_push_conversation("thread", "turn")
            .await
            .unwrap()
            .is_none());
        sqlx::query("UPDATE runtime_bindings SET provider_store=(SELECT provider_store FROM project_conversations WHERE conversation_id='parent') WHERE conversation_id='parent'").execute(&store.pool).await.unwrap();
        sqlx::query("UPDATE projects SET is_included=0")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .approval_push_conversation("thread", "turn")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn work_done_outside_wonder_notifies_once_with_its_reply() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        for _ in 0..2 {
            store
                .enqueue_external_push(
                    "claude:s1:7:completed",
                    "parent",
                    "completed",
                    Some("All done."),
                )
                .await
                .unwrap();
        }
        store
            .enqueue_external_push("claude:s1:9:attention", "parent", "attention", None)
            .await
            .unwrap();
        store.distribute_push().await.unwrap();
        let completed = store.next_push().await.unwrap().unwrap();
        assert_eq!(completed.kind, "completed");
        let preview = store.push_preview(&completed).await.unwrap().unwrap();
        assert_eq!(
            (preview.title.as_str(), preview.body.as_str()),
            ("Project", "All done.")
        );
        let attempt = store.begin_push_attempt(completed).await.unwrap().unwrap();
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        let attention = store.next_push().await.unwrap().unwrap();
        let preview = store.push_preview(&attention).await.unwrap().unwrap();
        assert_eq!(preview.title, "Project · Waiting on your computer");
        assert!(preview.body.starts_with("Claude is asking"));
        assert!(
            preview.approval.is_none(),
            "Desktop-owned requests cannot be answered on the phone"
        );
        let attempt = store.begin_push_attempt(attention).await.unwrap().unwrap();
        store
            .finish_push_attempt(&attempt, "delivered")
            .await
            .unwrap();
        assert!(store.next_push().await.unwrap().is_none());
        // A turn that failed or whose process quit on the Mac says so.
        for (key, body) in [
            ("codex:t:u:failed", "Codex could not finish"),
            ("claude:s1:10:ended", "Claude stopped on your computer, but"),
        ] {
            store
                .enqueue_external_push(key, "parent", "attention", None)
                .await
                .unwrap();
            store.distribute_push().await.unwrap();
            let ended = store.next_push().await.unwrap().unwrap();
            let preview = store.push_preview(&ended).await.unwrap().unwrap();
            assert_eq!(preview.title, "Project · Needs attention");
            assert!(preview.body.starts_with(body), "{}", preview.body);
            let attempt = store.begin_push_attempt(ended).await.unwrap().unwrap();
            store
                .finish_push_attempt(&attempt, "delivered")
                .await
                .unwrap();
        }
        // Nothing is recorded while no phone has notifications on.
        store.revoke_push("phone-a").await.unwrap();
        assert!(!store
            .enqueue_external_push("codex:t:u:completed", "parent", "completed", None)
            .await
            .unwrap());
    }

    /// Legacy pending Mac-dialog pushes also open the computer and do not claim
    /// their authorization thread is blocked or offer a consent response.
    #[tokio::test]
    async fn mac_dialog_preview_is_host_attention_without_a_chat_wait_claim() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        store
            .enqueue_external_push(
                "mac-permission:computer:fixture",
                "parent",
                "attention",
                None,
            )
            .await
            .unwrap();
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let preview = store.push_preview(&delivery).await.unwrap().unwrap();
        assert_eq!(preview.conversation_id, crate::HOST_VIEW_CONVERSATION_ID);
        assert_eq!(preview.title, "A Mac dialog needs you");
        assert_eq!(
            preview.body,
            "Open Computer in Cupola to view the dialog, or review it on your Mac."
        );
        assert!(preview.approval.is_none() && preview.question.is_none());
    }

    #[tokio::test]
    async fn notification_question_preserves_complete_single_choice_form() {
        let params = serde_json::json!({"_wonderSupportsQuestionReply":true,"isBlocking":true,
            "threadId":"thread","turnId":"turn","itemId":"question",
            "questions":[{"id":"tone:0","header":"Tone","question":"  Which tone?  ","isOther":true,"isSecret":false,
                "options":[{"label":"Brief","description":"One paragraph"},{"label":"Detailed","description":null}]}]});
        let question = notification_question(&params).unwrap();
        assert_eq!(question.id, "tone:0");
        assert_eq!(
            question.body,
            "Tone\n  Which tone?  \nBrief: One paragraph\nDetailed"
        );
        for (field, value) in [
            ("isOther", serde_json::json!(false)),
            ("isOther", serde_json::Value::Null),
            ("isSecret", serde_json::json!(true)),
            ("isSecret", serde_json::json!("false")),
            ("multiSelect", serde_json::json!(true)),
            ("multiSelect", serde_json::json!("false")),
            ("question", serde_json::json!("x".repeat(301))),
            ("question", serde_json::json!("First\nHidden")),
            ("question", serde_json::json!("Text\u{202e}hidden")),
            ("header", serde_json::json!("x".repeat(81))),
            ("id", serde_json::json!("")),
            ("id", serde_json::json!("a b")),
            ("id", serde_json::json!("x".repeat(257))),
            ("futureConsent", serde_json::json!("context")),
            (
                "options",
                serde_json::json!([{"label":"A", "description":"x".repeat(161)}]),
            ),
            (
                "options",
                serde_json::json!([{"label":"A", "image":"required"}]),
            ),
            (
                "options",
                serde_json::json!([{"label":"x".repeat(120)}, {"label":"y".repeat(120)}, {"label":"z".repeat(120)}]),
            ),
        ] {
            let mut changed = params.clone();
            changed["questions"][0][field] = value;
            assert!(
                notification_question(&changed).is_none(),
                "{field}: {changed}"
            );
        }
        for changes in [
            serde_json::json!({"_wonderSupportsQuestionReply":false}),
            serde_json::json!({"threadId":""}),
            serde_json::json!({"turnId":"a b"}),
            serde_json::json!({"itemId":null}),
            serde_json::json!({"isBlocking":"false"}),
            serde_json::json!({"kind":"unknown"}),
            serde_json::json!({"questions":[]}),
            serde_json::json!({"futureForm":{}}),
            serde_json::json!({"_wonderQuestionIncomplete":true}),
            serde_json::json!({"questions":[params["questions"][0],params["questions"][0]]}),
        ] {
            let mut changed = params.clone();
            changed
                .as_object_mut()
                .unwrap()
                .extend(changes.as_object().unwrap().clone());
            assert!(notification_question(&changed).is_none());
        }
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        bind_project_fixture(&store, "parent", AgentFamily::Codex, "thread", None, "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_message("phone-a", "question-owner", "Work", "hash", "parent", "now")
            .await
            .unwrap()
        else {
            panic!()
        };
        store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        let mut params = params;
        params["threadId"] = serde_json::json!("thread");
        params["turnId"] = serde_json::json!("turn");
        params["itemId"] = serde_json::json!("question");
        store
            .insert_pending_approval(
                "question",
                "item/tool/requestUserInput",
                &params.to_string(),
                "now",
            )
            .await
            .unwrap();
        let original = store.approval("question").await.unwrap().unwrap();
        let mut replacement = params.clone();
        replacement["questions"][0]["isSecret"] = true.into();
        store
            .insert_pending_approval(
                "question",
                "item/tool/requestUserInput",
                &replacement.to_string(),
                "later",
            )
            .await
            .unwrap();
        let retained = store.approval("question").await.unwrap().unwrap();
        assert_eq!(
            retained.params_json, original.params_json,
            "The encrypted option route relies on immutable request parameters"
        );
        assert_eq!(retained.action_nonce, original.action_nonce);
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let preview = store.push_preview(&delivery).await.unwrap().unwrap();
        assert_eq!(preview.body, "Tone\n  Which tone?  ");
        let payload = serde_json::to_value(&preview).unwrap();
        assert_eq!(payload["question"]["questionOptions"][0]["label"], "Brief");
        assert_eq!(
            payload["question"]["questionOptions"][0]["description"],
            "One paragraph"
        );
        assert!(preview.approval.is_none());
        let reference = preview.question.unwrap();
        assert_eq!(reference.question_id.as_deref(), Some("tone:0"));
        assert_eq!(reference.question_options.unwrap()[0].label, "Brief");
        assert!(!reference.supports_deny_message);
        assert_eq!(
            reference.action_nonce,
            store
                .approval("question")
                .await
                .unwrap()
                .unwrap()
                .action_nonce
        );
        let mut expanded = params.clone();
        expanded["questions"][0]["options"] = serde_json::json!([
            {"label":"One"},{"label":"Two"},{"label":"Three"},{"label":"Four"},{"label":"Five"}
        ]);
        sqlx::query("UPDATE approvals SET params_json=? WHERE server_request_id='question'")
            .bind(expanded.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let expanded_preview = store.push_preview(&delivery).await.unwrap().unwrap();
        let options = expanded_preview.question.unwrap().question_options.unwrap();
        assert_eq!(options.len(), 5);
        assert_eq!(options[4].label, "Five");
        // Unsupported forms remain visible without inventing a notification
        // choice or advertising an incomplete answer action.
        for field in ["options", "multiSelect"] {
            let mut unsupported = params.clone();
            unsupported["questions"][0][field] = if field == "options" {
                serde_json::json!([])
            } else {
                true.into()
            };
            sqlx::query("UPDATE approvals SET params_json=? WHERE server_request_id='question'")
                .bind(unsupported.to_string())
                .execute(&store.pool)
                .await
                .unwrap();
            assert!(
                store
                    .push_preview(&delivery)
                    .await
                    .unwrap()
                    .unwrap()
                    .question
                    .is_none(),
                "{field}"
            );
        }
        sqlx::query("UPDATE approvals SET state='resolved'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.push_preview(&delivery).await.unwrap().is_none());
    }

    #[test]
    fn notification_question_options_are_never_truncated() {
        let unicode = serde_json::json!({"_wonderSupportsQuestionReply":true,"isBlocking":true,
            "threadId":"thread","turnId":"turn","itemId":"question",
            "questions":[{"id":"q","question":"Which option?","isOther":true,
                "options":[{"label":"👩‍👩‍👧 Family","description":"日本語の説明"}]}]});
        assert_eq!(
            notification_option_question(&unicode).unwrap().options[0].label,
            "👩‍👩‍👧 Family"
        );
        let mut duplicate = unicode.clone();
        duplicate["questions"][0]["options"] =
            serde_json::json!([{"label":"Same"},{"label":"Same"}]);
        assert!(notification_option_question(&duplicate).is_none());
        assert!(
            notification_question(&duplicate).is_some(),
            "The legacy text-reply projection keeps its original contract"
        );
        for count in [1, 2, 3, 4, 5, 9, 32] {
            let options: Vec<_> = (0..count)
                .map(|index| serde_json::json!({"label":format!("Option {index}"),"description":"Details"}))
                .collect();
            let params = serde_json::json!({"_wonderSupportsQuestionReply":true,"isBlocking":true,
                "threadId":"thread","turnId":"turn","itemId":"question",
                "questions":[{"id":"q","question":"Which option?","isOther":true,"options":options}]});
            let question = notification_option_question(&params).unwrap();
            assert_eq!(question.options.len(), count);
            assert_eq!(
                question.options.last().unwrap().label,
                format!("Option {}", count - 1)
            );
            assert_eq!(question.body, "Which option?");
            if count > 4 {
                assert!(
                    notification_question(&params).is_none(),
                    "The legacy API capability must not expand"
                );
            }
            let mut multi = params.clone();
            multi["questions"][0]["multiSelect"] = true.into();
            assert!(notification_option_question(&multi).is_none());
        }
    }

    // Contract: notification actions keep their one-time authority even when
    // OpenCode offers remembered alternatives; incomplete consent stays in-app.
    #[tokio::test]
    async fn notification_commands_require_complete_simple_consent() {
        let store = store().await;
        store.revoke_push("phone-b").await.unwrap();
        store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "command-provider",
                project_id: "project-parent",
                family: AgentFamily::OpenCode,
                provider_store: "fixture-store",
                native_session_id: None,
                cwd: "/tmp",
                roots_revision: 1,
                title: "Project",
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
        bind_project_fixture(
            &store,
            "command-provider",
            AgentFamily::OpenCode,
            "opencode-thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_message(
                "phone-a",
                "command",
                "Work",
                "hash",
                "command-provider",
                "now",
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("opencode-thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        insert_approval(
            &store,
            "simple",
            "opencode-thread",
            "turn",
            r#","cwd":"/workspace/project","kind":"command""#,
        )
        .await;
        store.distribute_push().await.unwrap();
        let delivery = store.next_push().await.unwrap().unwrap();
        let original = store.approval("simple").await.unwrap().unwrap();
        let params: serde_json::Value = serde_json::from_str(&original.params_json).unwrap();
        let allow = serde_json::json!({"opencodePermission":{"optionId":"allow-rule", "kind":"allow_always"}});
        let deny = serde_json::json!({"opencodePermission":{"optionId":"deny-rule", "kind":"reject_always"}});
        let mut opencode = params.clone();
        opencode["agentFamily"] = serde_json::json!("opencode");
        opencode["availableDecisions"] =
            serde_json::json!(["accept", "decline", allow, deny, "cancel"]);
        let mut capable = params.clone();
        capable["supportsDenyMessage"] = serde_json::json!(true);
        let mut cases = vec![
            ("default one-time choices".to_owned(), params.clone(), true),
            ("typed denial".to_owned(), capable, true),
            (
                "OpenCode remembered alternatives".to_owned(),
                opencode.clone(),
                true,
            ),
        ];
        for (label, choices) in [
            (
                "session",
                serde_json::json!(["accept", "acceptForSession", "decline", "cancel"]),
            ),
            (
                "allow rule",
                serde_json::json!(["accept", "decline", allow, "cancel"]),
            ),
            ("deny rule", serde_json::json!(["accept", "decline", deny])),
            (
                "same kind distinct IDs",
                serde_json::json!(["accept", "decline", deny, {"opencodePermission":{"optionId":"other-deny", "kind":"reject_always"}}]),
            ),
        ] {
            let mut changed = opencode.clone();
            changed["availableDecisions"] = choices;
            cases.push((label.to_owned(), changed, true));
        }
        // Exercise both old simple consent and the new structured alternatives
        // against the same content guards, through the persisted preview path.
        for (base_label, base) in [("simple", &params), ("OpenCode", &opencode)] {
            for (field, value) in [
                ("command", serde_json::json!("x".repeat(121))),
                ("command", serde_json::json!("echo first\necho hidden")),
                ("command", serde_json::json!("echo \u{202e}hidden")),
                ("cwd", serde_json::Value::Null),
                ("cwd", serde_json::json!("x".repeat(101))),
                ("reason", serde_json::json!("x".repeat(161))),
                ("kind", serde_json::json!("writeStdin")),
                ("kind", serde_json::json!("tool")),
                ("kind", serde_json::Value::Null),
                ("environmentId", serde_json::json!("remote")),
                ("questions", serde_json::json!([{ "isSecret": true }])),
                ("changes", serde_json::json!([])),
                (
                    "additionalPermissions",
                    serde_json::json!({"network":{"enabled":true}}),
                ),
                (
                    "availableDecisions",
                    serde_json::json!(["accept", "cancel"]),
                ),
                (
                    "availableDecisions",
                    serde_json::json!(["decline", "cancel"]),
                ),
                (
                    "availableDecisions",
                    serde_json::json!(["accept", "decline", {"unknown":{}}]),
                ),
                ("futureConsentDetail", serde_json::json!("must be reviewed")),
            ] {
                let mut changed = base.clone();
                changed[field] = value;
                cases.push((format!("{base_label}: {field}"), changed, false));
            }
        }
        for choice in [
            serde_json::json!({"opencodePermission":{"optionId":"", "kind":"allow_always"}}),
            serde_json::json!({"opencodePermission":{"optionId":"  ", "kind":"allow_always"}}),
            serde_json::json!({"opencodePermission":{"optionId":42, "kind":"allow_always"}}),
            serde_json::json!({"opencodePermission":{"kind":"allow_always"}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule"}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule", "kind":null}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule", "kind":"unknown"}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule", "kind":"allow_once"}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule", "kind":"allow_always", "permissions":true}}),
            serde_json::json!({"opencodePermission":{"optionId":"rule", "kind":"reject_always"}, "other":true}),
            serde_json::json!({"opencodePermission":"rule"}),
            serde_json::json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["*"]}}),
            serde_json::json!({"applyNetworkPolicyAmendment":{"network_policy_amendment":{}}}),
            serde_json::Value::Null,
        ] {
            let mut changed = opencode.clone();
            changed["availableDecisions"] = serde_json::json!(["accept", "decline", choice]);
            cases.push((
                format!("malformed or richer choice: {choice}"),
                changed,
                false,
            ));
        }
        for choices in [
            serde_json::json!([allow, deny, "cancel"]),
            serde_json::json!(["accept", allow, deny]),
            serde_json::json!(["decline", allow, deny]),
            serde_json::json!(["accept", "decline", allow, allow]),
            serde_json::json!(["accept", "decline", allow, {"opencodePermission":{"optionId":"allow-rule", "kind":"reject_always"}}]),
            serde_json::json!({"accept":true, "decline":true}),
        ] {
            let mut changed = opencode.clone();
            changed["availableDecisions"] = choices.clone();
            cases.push((
                format!("missing or ambiguous choices: {choices}"),
                changed,
                false,
            ));
        }
        for family in [
            serde_json::json!("codex"),
            serde_json::json!("claude"),
            serde_json::Value::Null,
        ] {
            let mut changed = opencode.clone();
            changed["agentFamily"] = family.clone();
            cases.push((
                format!("foreign or missing provider: {family}"),
                changed,
                false,
            ));
        }
        // Cursor uses the same one-time notification contract with its own
        // advertised remembered choices. Reuse every persisted-preview guard.
        let cursor_cases = cases
            .iter()
            .filter(|(_, params, _)| params["agentFamily"] == "opencode")
            .map(|(label, params, actionable)| {
                let mut cursor = params.clone();
                cursor["agentFamily"] = serde_json::json!("cursor");
                if let Some(choices) = cursor["availableDecisions"].as_array_mut() {
                    for choice in choices {
                        if let Some(object) = choice.as_object_mut() {
                            if let Some(option) = object.remove("opencodePermission") {
                                object.insert("acpPermission".into(), option);
                            }
                        }
                    }
                }
                (format!("Cursor: {label}"), cursor, *actionable)
            })
            .collect::<Vec<_>>();
        cases.extend(cursor_cases);
        let mut wrong_namespace = opencode.clone();
        wrong_namespace["agentFamily"] = serde_json::json!("cursor");
        cases.push((
            "Cursor rejects OpenCode choices".into(),
            wrong_namespace,
            false,
        ));
        let mut wrong_namespace = opencode.clone();
        wrong_namespace["availableDecisions"] = serde_json::json!([
            "accept", "decline", {"acpPermission":{"optionId":"rule", "kind":"allow_always"}}
        ]);
        cases.push((
            "OpenCode rejects Cursor choices".into(),
            wrong_namespace,
            false,
        ));
        let case_count = cases.len();
        for (label, changed, actionable) in cases {
            sqlx::query("UPDATE approvals SET params_json=? WHERE approval_id=?")
                .bind(changed.to_string())
                .bind(&original.approval_id)
                .execute(&store.pool)
                .await
                .unwrap();
            let stored = store
                .approval(&original.approval_id)
                .await
                .unwrap()
                .unwrap();
            let preview = store.push_preview(&delivery).await.unwrap().unwrap();
            assert_eq!(
                preview.approval.is_some(),
                actionable,
                "stored push preview: {label}"
            );
            assert_eq!(
                notification_command(&stored).is_some(),
                actionable,
                "{label}"
            );
            if let Some(reference) = &preview.approval {
                assert_eq!(
                    preview.body, "Install dependencies?\nnpm ci\n/workspace/project",
                    "{label}"
                );
                assert_eq!(reference.approval_id, original.approval_id, "{label}");
                assert_eq!(reference.action_nonce, original.action_nonce, "{label}");
                assert!(reference.question_id.is_none(), "{label}");
                assert_eq!(
                    reference.supports_deny_message,
                    changed["supportsDenyMessage"] == true,
                    "{label}"
                );
                // The preview carries only the existing action reference. It
                // cannot substitute any remembered decision or provider ID.
                let expected = serde_json::json!({
                    "approvalId": original.approval_id,
                    "actionNonce": original.action_nonce,
                    "supportsDenyMessage": reference.supports_deny_message,
                });
                let serialized = serde_json::to_value(reference).unwrap();
                assert_eq!(serialized, expected, "{label}");
            }
            eprintln!("notification consent {label}: one-time actions={actionable}");
        }
        for method in [
            "item/fileChange/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
        ] {
            sqlx::query("UPDATE approvals SET method=?,params_json=? WHERE approval_id=?")
                .bind(method)
                .bind(opencode.to_string())
                .bind(&original.approval_id)
                .execute(&store.pool)
                .await
                .unwrap();
            let stored = store
                .approval(&original.approval_id)
                .await
                .unwrap()
                .unwrap();
            assert!(notification_command(&stored).is_none(), "{method}");
            assert!(
                store
                    .push_preview(&delivery)
                    .await
                    .unwrap()
                    .unwrap()
                    .approval
                    .is_none(),
                "{method}"
            );
        }
        for state in ["resolving", "resolved"] {
            let mut approval = original.clone();
            approval.state = state.into();
            assert!(notification_command(&approval).is_none(), "{state}");
        }
        eprintln!("notification consent: {case_count} stored preview cases and 3 non-command methods passed");
    }

    #[test]
    fn preview_excerpt_bounds_unicode_and_json_escaping() {
        let value = excerpt(&"🦀\"\n".repeat(1000), 1000);
        assert!(value.len() <= 1003);
        assert!(
            serde_json::to_vec(&PushPreview {
                title: excerpt(&"\"".repeat(200), 160),
                body: value,
                conversation_id: "thread".into(),
                approval: None,
                question: None,
                read_through: Some(PushReadThrough {
                    host_epoch: "e".repeat(64),
                    sequence: u64::MAX,
                }),
                open: None,
            })
            .unwrap()
            .len()
                < 2400
        );
    }
}
