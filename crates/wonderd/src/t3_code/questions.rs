//! T3 Code's questions on the phone (owner, Oct 22: "Approvals, yeah, I feel
//! like we should be able to"). A question the agent asks in a T3 thread
//! (Claude's AskUserQuestion, Codex's request for input) is listed by T3
//! Code's MCP tools (`t3_pending_request_list/read`) and answered with
//! `t3_pending_request_respond`, so it becomes the phone's usual question
//! item: a pending approval `item/tool/requestUserInput` keyed
//! `t3:{thread}:{request}`, routed to the conversation whose session the T3
//! thread is (`Store::t3_code_conversation`), answered through
//! `lib::resolve_approval` once the phone's signature is verified.
//!
//! T3 Code's tools cover questions only: "This cannot approve a permission
//! request", and its MCP grant carries no approval authority. A permission
//! T3 Code waits on stays on the computer, and the phone says so: the
//! thread's `pendingRequestCount` counts more requests than the questions
//! listed (`T3Code::approves_on_computer`).
//!
//! T3 Code pushes nothing to Cupola, so a T3 thread is read every `POLL`
//! while a turn runs in it (`watch`, started by a send into it and by the
//! phone reading a thread whose turn T3 Code runs), and only then. Each read
//! (`poll`) keeps the phone's items equal to T3 Code's list: a new question
//! is added once, one answered on the computer is closed, and every one is
//! closed when the turn ends. A read that fails (T3 Code renewing its
//! sign-in, or not answering) changes nothing; when the watch stops, what
//! it left open is settled once (`stopped`), as is what a restart left open
//! (`recheck`). One closed because T3 Code couldn't be read is offered again,
//! under a new nonce, once a read finds T3 Code still asks it.
use std::collections::HashSet;

use super::*;

const POLL: Duration = Duration::from_secs(5);
/// Failed reads in a row before a watch stops (about a minute); the next
/// send or phone read starts it again.
const GIVE_UP: u32 = 12;
const METHOD: &str = "item/tool/requestUserInput";
/// The decision a question closed by T3 Code (answered on the computer)
/// records, and one whose turn ended.
pub(crate) const RESOLVED_THERE: &str = "t3_code_resolved";
const TURN_ENDED: &str = "turn_ended";
/// A question left when a watch stopped without reaching T3 Code: only the
/// computer can answer it now (`ANSWER_ON_COMPUTER`).
pub(crate) const ON_COMPUTER: &str = "t3_code_on_computer";
pub(crate) const ANSWER_ON_COMPUTER: &str =
    "Answer this on your computer in T3 Code. Cupola couldn't reach T3 Code to send it.";

/// The T3 threads watched now and the conversations whose T3 turn waits on
/// a permission only the computer can give.
#[derive(Default)]
pub(crate) struct Questions {
    inner: Mutex<Watched>,
}

#[derive(Default)]
struct Watched {
    threads: HashSet<String>,
    on_computer: HashSet<String>,
    /// `POLL`, shorter in tests.
    every: Option<Duration>,
    /// Tool shape for the current connection epoch; reconsider after reconnect.
    api: Option<(u64, bool)>,
}

impl Questions {
    fn every(&self) -> Duration {
        self.inner.lock().unwrap().every.unwrap_or(POLL)
    }

    /// Whether the conversation's on-computer state changed.
    fn set_on_computer(&self, conversation: &str, waits: bool) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if waits {
            inner.on_computer.insert(conversation.to_owned())
        } else {
            inner.on_computer.remove(conversation)
        }
    }

    #[cfg(test)]
    pub(crate) fn poll_every(&self, every: Duration) {
        self.inner.lock().unwrap().every = Some(every);
    }

    pub(crate) fn watching(&self, thread: &str) -> bool {
        self.inner.lock().unwrap().threads.contains(thread)
    }
}

/// Match only the named missing tool, never a missing request or an ambiguous
/// execution error. T3 versions vary quotation and append available tools.
fn missing_tool(message: &str, tool: &str) -> bool {
    ['\'', '"'].into_iter().any(|quote| {
        message
            .strip_prefix(&format!("Tool {quote}{tool}{quote} not found"))
            .is_some_and(|tail| tail.is_empty() || tail.starts_with('.') || tail.starts_with(':'))
    })
}

impl T3Code {
    /// Current T3 consolidates the question tools. Fall back only after an
    /// explicit missing-tool response, which guarantees no answer was applied.
    /// Never retry an ambiguous/lost answer through a different API shape.
    async fn pending_request(
        &self,
        action: &str,
        mut arguments: Value,
    ) -> Result<Value, CallError> {
        let epoch = self.inner.lock().unwrap().epoch;
        let consolidated = self
            .questions
            .inner
            .lock()
            .unwrap()
            .api
            .filter(|(known, _)| *known == epoch)
            .is_some_and(|(_, action)| action);
        let legacy = format!("t3_pending_request_{action}");
        let tool = if consolidated {
            "t3_pending_request"
        } else {
            &legacy
        };
        if consolidated {
            arguments["action"] = json!(action);
        }
        let result = self.call(tool, arguments.clone()).await;
        match result {
            Err(CallError::Protocol(message)) if missing_tool(&message, tool) => {
                // A running T3 installation can upgrade or downgrade. Only an
                // explicit missing-tool response permits trying the other shape.
                let alternate = if consolidated {
                    arguments.as_object_mut().unwrap().remove("action");
                    legacy.as_str()
                } else {
                    arguments["action"] = json!(action);
                    "t3_pending_request"
                };
                let result = self.call(alternate, arguments).await;
                if result.is_ok() {
                    self.questions.inner.lock().unwrap().api = Some((epoch, !consolidated));
                }
                result
            }
            result => {
                if result.is_ok() {
                    self.questions.inner.lock().unwrap().api = Some((epoch, consolidated));
                }
                result
            }
        }
    }

    /// The conversation's T3 turn waits on a permission T3 Code asks for on
    /// the computer, which the phone can't give.
    pub(crate) fn approves_on_computer(&self, conversation: &str) -> bool {
        self.questions
            .inner
            .lock()
            .unwrap()
            .on_computer
            .contains(conversation)
    }
}

/// The phone's request ID for T3 Code's request `request` in `thread`.
pub(crate) fn request_key(thread: &str, request: &str) -> String {
    format!("t3:{thread}:{request}")
}

fn thread_prefix(thread: &str) -> String {
    format!("t3:{thread}:")
}

/// A request T3 Code asked (`request_key`), not a provider runtime: none
/// is named in its parameters.
pub(crate) fn asked_in_t3_code(server_request_id: &str, params: &Value) -> bool {
    server_request_id.starts_with("t3:") && params.get("_wonderRuntimeId").is_none()
}

/// Ends a watch however its task ends.
struct Watch(Arc<T3Code>, String);

impl Drop for Watch {
    fn drop(&mut self) {
        self.0
            .questions
            .inner
            .lock()
            .unwrap()
            .threads
            .remove(&self.1);
    }
}

/// Reads `thread` every `POLL` while a turn runs in it (one watch per T3
/// thread), keeping the conversation's questions equal to T3 Code's.
pub(crate) fn watch(state: &AppState, conversation: &str, thread: &str) {
    let t3 = state.t3_code.clone();
    if !t3
        .questions
        .inner
        .lock()
        .unwrap()
        .threads
        .insert(thread.to_owned())
    {
        return;
    }
    let watch = Watch(t3, thread.to_owned());
    let (state, conversation, thread) = (state.clone(), conversation.to_owned(), thread.to_owned());
    tokio::spawn(async move {
        let _watch = watch;
        let mut failures = 0;
        loop {
            match poll(&state, &conversation, &thread).await {
                Ok(true) => failures = 0,
                Ok(false) | Err(CallError::NotConnected) => break,
                Err(_) => {
                    failures += 1;
                    if failures >= GIVE_UP {
                        break;
                    }
                }
            }
            tokio::time::sleep(state.t3_code.questions.every()).await;
        }
        stopped(&state, &conversation, &thread).await;
        on_computer(&state, &conversation, false).await;
    });
}

/// A watch stopped (its turn ended, it gave up, or T3 Code no longer
/// accepts Cupola): a question still open is read back once. One T3 Code
/// no longer asks was answered on the computer; one it still asks stays
/// answerable; if T3 Code can't be read, the computer is the only place
/// left to answer, and the phone stops offering it.
pub(crate) async fn stopped(state: &AppState, conversation: &str, thread: &str) {
    let Ok(waiting) = state
        .store
        .pending_approvals_with_prefix(&thread_prefix(thread))
        .await
    else {
        return;
    };
    settle(state, conversation, thread, &waiting).await;
}

/// After a restart nothing watches T3 Code yet: the questions the phone
/// still offers in `thread` (`waiting`, by the nonce each was read under)
/// are read back once, as when a watch stops. A watch running now owns them.
pub(crate) async fn recheck(
    state: &AppState,
    conversation: &str,
    thread: &str,
    waiting: &[(String, String)],
) {
    if !state.t3_code.questions.watching(thread) {
        settle(state, conversation, thread, waiting).await;
    }
}

/// Settles `waiting` against T3 Code's list, read once. A question offered
/// again since it was read (a new nonce) is the newer read's to settle.
async fn settle(state: &AppState, conversation: &str, thread: &str, waiting: &[(String, String)]) {
    if waiting.is_empty() {
        return;
    }
    let listed = listed(&state.t3_code, thread).await;
    for (key, nonce) in waiting {
        let decision = match &listed {
            Ok(ids) if ids.iter().any(|id| &request_key(thread, id) == key) => continue,
            Ok(_) => RESOLVED_THERE,
            Err(_) => ON_COMPUTER,
        };
        if state
            .store
            .retire_pending_approval_offered(key, nonce, decision, &now_ms().to_string())
            .await
            .unwrap_or(false)
        {
            resolved(state, conversation, key, decision).await;
        }
    }
}

async fn on_computer(state: &AppState, conversation: &str, waits: bool) {
    if state.t3_code.questions.set_on_computer(conversation, waits) {
        crate::external_push::publish_status(state, conversation).await;
    }
}

/// One read of `thread`: its questions become the phone's, those T3 Code
/// no longer asks are closed. False once no turn runs there (every question
/// is closed then) or the conversation is gone.
pub(crate) async fn poll(
    state: &AppState,
    conversation: &str,
    thread: &str,
) -> Result<bool, CallError> {
    let t3 = &state.t3_code;
    let Ok(Some(stored)) = state.store.project_conversation(conversation).await else {
        return Ok(false);
    };
    let Some(native) = stored.native_session_id else {
        return Ok(false);
    };
    let page = t3
        .call(
            "t3_thread_read",
            json!({"threadId": thread, "view": "messages", "limit": 1}),
        )
        .await?;
    let detail = page
        .get("thread")
        .ok_or_else(|| CallError::Protocol("no thread".into()))?;
    let Ok(open) = state
        .store
        .open_approvals_with_prefix(&thread_prefix(thread))
        .await
    else {
        return Ok(true);
    };
    let Some(run) = detail.get("activeRunId").and_then(Value::as_str) else {
        // The turn ended: nothing it asked can be answered any more.
        for (key, _) in open {
            if let Ok(true) = state
                .store
                .resolve_approval_by_server_request_id(&key, TURN_ENDED, &now_ms().to_string())
                .await
            {
                resolved(state, conversation, &key, TURN_ENDED).await;
            }
        }
        on_computer(state, conversation, false).await;
        return Ok(false);
    };
    // T3 Code counts its permission requests too; none waiting, no list.
    let pending = detail.get("pendingRequestCount").and_then(Value::as_u64);
    let listed = if pending == Some(0) {
        Vec::new()
    } else {
        listed(t3, thread).await?
    };
    for request in &listed {
        let key = request_key(thread, request);
        // One closed when Cupola couldn't reach T3 Code, which still asks
        // it, is offered again as T3 Code reads it now.
        let reopens = match state.store.approval(&key).await {
            Ok(None) => false,
            Ok(Some(stored)) if stored.decision.as_deref() == Some(ON_COMPUTER) => true,
            _ => continue,
        };
        let read = match t3
            .pending_request("read", json!({"threadId": thread, "requestId": request}))
            .await
        {
            Ok(read) => read,
            // Answered or gone since it was listed.
            Err(CallError::Refused { .. }) => continue,
            Err(error) => return Err(error),
        };
        let Some(params) = question_item(thread, run, &read) else {
            continue;
        };
        // Its questions belong to this conversation (`approval_push_conversation`).
        if state
            .store
            .t3_code_thread(conversation, &native)
            .await
            .ok()
            .flatten()
            .as_deref()
            != Some(thread)
            && state
                .store
                .set_t3_code_thread(conversation, &native, thread, &crate::projects::now_text())
                .await
                .is_err()
        {
            continue;
        }
        let opened = if reopens {
            state
                .store
                .reopen_lost_approval(&key, ON_COMPUTER, &params.to_string())
                .await
                .unwrap_or(false)
        } else {
            state
                .store
                .insert_pending_approval(&key, METHOD, &params.to_string(), &now_ms().to_string())
                .await
                .is_ok()
        };
        if opened {
            let _ = crate::publish_event_with_context(
                state,
                wonder_api::WonderEvent::ApprovalOpened {
                    request_id: key.clone(),
                },
                crate::EventContext {
                    request_id: Some(key.clone()),
                    conversation_id: Some(conversation.to_owned()),
                    approval_id: Some(key),
                    ..crate::EventContext::default()
                },
            )
            .await;
        }
    }
    // Answered on the computer. One the phone is answering right now
    // (`resolving`) is the answer's to close.
    for (key, _) in open.iter().filter(|(_, state)| state == "pending") {
        let asked = listed
            .iter()
            .any(|request| &request_key(thread, request) == key);
        if !asked
            && state
                .store
                .retire_pending_approval(key, RESOLVED_THERE, &now_ms().to_string())
                .await
                .unwrap_or(false)
        {
            resolved(state, conversation, key, RESOLVED_THERE).await;
        }
    }
    let waits = pending.is_some_and(|pending| pending as usize > listed.len());
    on_computer(state, conversation, waits).await;
    Ok(true)
}

async fn resolved(state: &AppState, conversation: &str, key: &str, decision: &str) {
    let _ = crate::publish_event_with_context(
        state,
        wonder_api::WonderEvent::ApprovalResolved {
            request_id: key.to_owned(),
            decision: decision.to_owned(),
        },
        crate::EventContext {
            request_id: Some(key.to_owned()),
            conversation_id: Some(conversation.to_owned()),
            approval_id: Some(key.to_owned()),
            ..crate::EventContext::default()
        },
    )
    .await;
}

/// The questions T3 Code asks in `thread` now, by T3 Code's request ID.
async fn listed(t3: &T3Code, thread: &str) -> Result<Vec<String>, CallError> {
    let list = t3
        .pending_request("list", json!({"threadId": thread}))
        .await?;
    Ok(list
        .get("requestIds")
        .and_then(Value::as_array)
        .ok_or_else(|| CallError::Protocol("no requestIds".into()))?
        .iter()
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect())
}

/// T3 Code's request (`t3_pending_request_read`) as the phone's question
/// item, with what its answer needs kept aside (`_wonderT3Code`, never sent
/// to the phone): each option's value by its label.
fn question_item(thread: &str, run: &str, read: &Value) -> Option<Value> {
    let request = read.get("requestId")?.as_str()?;
    let asked = read.get("questions")?.as_array()?;
    if asked.is_empty() {
        return None;
    }
    let mut values = serde_json::Map::new();
    let mut questions = Vec::with_capacity(asked.len());
    for question in asked {
        let id = question.get("id")?.as_str()?;
        let text = question.get("question")?.as_str()?;
        let mut by_label = serde_json::Map::new();
        let options: Vec<Value> = question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| {
                let label = option.get("label")?.as_str()?;
                let value = option.get("value").and_then(Value::as_str).unwrap_or(label);
                by_label.insert(label.to_owned(), value.into());
                let mut shown = json!({"label": label});
                if let Some(description) = option.get("description").and_then(Value::as_str) {
                    shown["description"] = description.into();
                }
                Some(shown)
            })
            .collect();
        values.insert(id.to_owned(), by_label.into());
        let mut item = json!({
            "id": id,
            "question": text,
            "options": options,
            "multiSelect": question.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
            // An open question takes the owner's own words.
            "isOther": question.get("allowCustomAnswer").and_then(Value::as_bool)
                .unwrap_or(options.is_empty()),
            "isSecret": false,
        });
        if let Some(header) = question
            .get("header")
            .and_then(Value::as_str)
            .filter(|header| !header.is_empty())
        {
            item["header"] = header.into();
        }
        questions.push(item);
    }
    Some(json!({
        "threadId": thread,
        "turnId": run,
        "itemId": request,
        "isBlocking": true,
        "questions": questions,
        "_wonderT3Code": {"threadId": thread, "requestId": request, "values": values},
    }))
}

/// The phone's answer (`{"answers": {id: {"answers": [label or text]}}}`)
/// as T3 Code takes it: each question's option value, or the owner's own
/// words, and a list only where the question takes several.
fn t3_answers(params: &Value, response: &Value) -> Option<Value> {
    let values = &params["_wonderT3Code"]["values"];
    let questions = params.get("questions")?.as_array()?;
    let mut answers = serde_json::Map::new();
    for (id, answer) in response.get("answers")?.as_object()? {
        let chosen = answer
            .get("answers")?
            .as_array()?
            .iter()
            .map(|chosen| {
                let chosen = chosen.as_str()?;
                Some(values[id][chosen].as_str().unwrap_or(chosen).to_owned())
            })
            .collect::<Option<Vec<_>>>()?;
        let several = questions
            .iter()
            .find(|question| question["id"] == id.as_str())?
            .get("multiSelect")
            == Some(&Value::Bool(true));
        answers.insert(
            id.clone(),
            if several {
                json!(chosen)
            } else {
                json!(chosen.into_iter().next()?)
            },
        );
    }
    Some(answers.into())
}

/// What became of the phone's answer to a question T3 Code asked.
#[derive(Debug, PartialEq)]
pub(crate) enum Answered {
    Sent,
    /// T3 Code no longer asks it: answered on the computer, or its turn ended.
    Gone,
    /// Not answered. `sent_nothing`: certainly never reached T3 Code, so
    /// the phone may answer anew; otherwise the same answer is retried.
    Failed {
        message: &'static str,
        sent_nothing: bool,
    },
}

pub(crate) const GONE: &str =
    "T3 Code no longer asks this. It was answered on your computer, or its reply ended.";

/// Sends the phone's verified answer (`response`) to T3 Code. A refused or
/// lost answer is checked against T3 Code's list: a question it no longer
/// asks was answered (by this answer when it may have arrived, `recovering`
/// included, otherwise on the computer).
pub(crate) async fn answer(
    state: &AppState,
    params: &Value,
    response: &Value,
    recovering: bool,
) -> Answered {
    let t3 = &state.t3_code;
    let asked = &params["_wonderT3Code"];
    let (Some(thread), Some(request), Some(answers)) = (
        asked["threadId"].as_str(),
        asked["requestId"].as_str(),
        t3_answers(params, response),
    ) else {
        return Answered::Failed {
            message: "This answer doesn't fit T3 Code's question.",
            sent_nothing: true,
        };
    };
    let error = match t3
        .pending_request(
            "respond",
            json!({"threadId": thread, "requestId": request, "answers": answers}),
        )
        .await
    {
        Ok(_) => return Answered::Sent,
        Err(error) => error,
    };
    if let Ok(ids) = listed(t3, thread).await {
        if !ids.iter().any(|id| id == request) {
            return if error.unconfirmed() || recovering {
                Answered::Sent
            } else {
                Answered::Gone
            };
        }
    }
    Answered::Failed {
        message: match error {
            CallError::NotConnected | CallError::Unauthorized | CallError::Forbidden => {
                "T3 Code no longer accepts Cupola. Connect T3 Code again in Settings, then answer again."
            }
            CallError::Offline | CallError::Lost => {
                "T3 Code on your computer didn't answer. Try again shortly."
            }
            _ => "T3 Code didn't take this answer. Try again.",
        },
        sent_nothing: !error.unconfirmed(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_t3_question_reads_as_the_phones_question_and_its_answer_as_t3s() {
        let read = json!({"requestId": "req-1", "questions": [
            {"id": "tone", "header": "Tone", "question": "Which tone?", "options": [
                {"label": "Brief", "description": "A line or two", "value": "brief"},
                {"label": "Thorough", "description": "Every step"}]},
            {"id": "parts", "header": "", "question": "Which parts?", "multiSelect": true, "options": [
                {"label": "Intro", "description": "", "value": "intro"},
                {"label": "Summary", "description": ""}]},
            {"id": "name", "header": "Name", "question": "What should it be called?", "options": []}]});
        let params = question_item("t3-thread", "run-1", &read).unwrap();
        assert_eq!(params["threadId"], "t3-thread");
        assert_eq!(params["turnId"], "run-1");
        assert_eq!(params["itemId"], "req-1");
        let summary = crate::redact_approval_params(METHOD, &params);
        assert_eq!(
            summary["questions"],
            json!([
                {"id": "tone", "header": "Tone", "question": "Which tone?", "isOther": false,
                    "isSecret": false, "multiSelect": false, "options": [
                    {"label": "Brief", "description": "A line or two"},
                    {"label": "Thorough", "description": "Every step"}]},
                {"id": "parts", "question": "Which parts?", "isOther": false, "isSecret": false,
                    "multiSelect": true, "options": [
                    {"label": "Intro", "description": ""}, {"label": "Summary", "description": ""}]},
                {"id": "name", "header": "Name", "question": "What should it be called?",
                    "isOther": true, "isSecret": false, "multiSelect": false, "options": []}])
        );
        assert!(
            summary.get("_wonderT3Code").is_none(),
            "what the answer needs stays on the computer"
        );
        let response = json!({"answers": {
            "tone": {"answers": ["Brief"]},
            "parts": {"answers": ["Intro", "Summary"]},
            "name": {"answers": ["Release notes"]}}});
        assert_eq!(
            t3_answers(&params, &response).unwrap(),
            json!({"tone": "brief", "parts": ["intro", "Summary"], "name": "Release notes"})
        );
        let unknown = json!({"answers": {"other": {"answers": ["x"]}}});
        assert_eq!(t3_answers(&params, &unknown), None);
        assert_eq!(
            question_item(
                "t3-thread",
                "run-1",
                &json!({"requestId": "r", "questions": []})
            ),
            None
        );
    }
}
