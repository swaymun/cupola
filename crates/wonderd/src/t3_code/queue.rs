//! A message T3 Code queued shows as queued on the phone (owner, Oct 22:
//! "on T3 Code I see that it is a queued message ... show it as a queued
//! message in Cupola too, and allow the option to steer").
//!
//! T3 Code answers `t3_thread_send` with how it took the message
//! (`delivery`: started, steered, restarted or queued) and, for a queued
//! one, the queued run's ID (`runId`, the `queuedRunId` of its queue tools;
//! read in T3 Code 0.1.1's `ThreadManagementService.sendToThread`). Cupola's
//! message is delivered (`completed`: T3 Code has it, durably) and its
//! intent keeps that run as `t3QueuedRunId` for as long as it waits there.
//!
//! The invariant: a message is listed in its conversation's queue
//! (`queue::list`, so the phone shows it in its Queued dock with Send now,
//! Edit and Cancel) exactly while T3 Code's queue has its run. It leaves the
//! queue once, by the first of:
//!
//! - T3 Code starts it (its turn began, or it steered the running one): its
//!   timeline has the message (`Sent::is`). It shows as sent, in its place
//!   in history from then (`Store::t3_code_queued_left`).
//! - It was cancelled, in T3 Code or from the phone: neither in the queue
//!   nor in the timeline. It is `interrupted`, as a message cancelled from
//!   Cupola's own queue is.
//!
//! The queue is read before the timeline: a run that left the queue before
//! that read either started (and the timeline read after it has it) or was
//! cancelled. T3 Code applies each command whole, so no read sees a run in
//! neither place that later starts.
//!
//! T3 Code pushes nothing to Cupola. Its queue is read only while a message
//! waits there, after a history refresh of the conversation (T3 Code's run
//! writes the session Cupola reads) and when the phone reads the queue, at
//! most once every `CHECK_EVERY` per conversation and in a task of its own;
//! a read that fails waits twice as long each time (up to `CHECK_BACKOFF_MAX`)
//! and changes nothing. A restart keeps the state: it is in the intent.
//!
//! The phone's Send now, Edit and Cancel go to T3 Code's queue tools
//! (`t3_queue_promote_to_steer` into the run active now, `t3_queue_edit`,
//! `t3_queue_cancel`) and change Cupola's message only once T3 Code took the
//! change. One whose answer was lost is settled by the next read.
use std::collections::BTreeMap;

use super::*;

/// The least time between two reads of one conversation's T3 queue.
const CHECK_EVERY: Duration = Duration::from_secs(5);
const CHECK_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// The conversations whose T3 queue is read now, or not before a time.
#[derive(Default)]
pub(crate) struct Checks {
    pub(super) inner: Mutex<HashMap<String, Check>>,
}

#[derive(Default)]
pub(super) struct Check {
    running: bool,
    failures: u32,
    next: Option<Instant>,
}

/// A Cupola message waiting in T3 Code's queue.
pub(crate) struct Queued {
    pub(crate) message: String,
    pub(crate) client: String,
    pub(crate) body: String,
    pub(crate) revision: i64,
    thread: String,
    run: String,
    t3_message: Option<String>,
}

/// The conversation's messages waiting in T3 Code's queue, in the order
/// sent. Reads Cupola's store only.
pub(crate) async fn queued(state: &AppState, conversation: &str) -> Vec<Queued> {
    let Ok(sends) = state.store.t3_code_sent_messages(conversation).await else {
        return Vec::new();
    };
    sends
        .into_iter()
        .filter(|send| send.state == "completed")
        .filter_map(|send| {
            let intent = serde_json::from_str::<Value>(&send.intent).ok()?;
            Some(Queued {
                thread: intent["t3ThreadId"].as_str()?.to_owned(),
                run: intent["t3QueuedRunId"].as_str()?.to_owned(),
                t3_message: intent["t3MessageId"].as_str().map(str::to_owned),
                message: send.id,
                client: send.client_message_id,
                body: send.body,
                revision: send.revision,
            })
        })
        .collect()
}

/// The queued run in T3 Code's answer to a send, when it queued the
/// message (`t3QueuedRunId`).
pub(crate) fn queued_run(result: &Value) -> Option<&str> {
    (result["delivery"] == "queued" || result["status"] == "queued")
        .then(|| result["runId"].as_str())
        .flatten()
}

/// Reads the conversation's T3 queue soon, in a task of its own, unless a
/// read runs or ran too recently (see the module comment).
pub(crate) fn check_soon(state: &AppState, conversation: &str) {
    if !state.t3_code.connected() {
        return;
    }
    {
        let mut checks = state.t3_code.queue.inner.lock().unwrap();
        let check = checks.entry(conversation.to_owned()).or_default();
        if check.running || check.next.is_some_and(|next| Instant::now() < next) {
            return;
        }
        check.running = true;
    }
    let state = state.clone();
    let conversation = conversation.to_owned();
    tokio::spawn(async move {
        let result = check(&state, &conversation).await;
        let mut checks = state.t3_code.queue.inner.lock().unwrap();
        let check = checks.entry(conversation).or_default();
        check.running = false;
        check.failures = if result.is_ok() {
            0
        } else {
            check.failures + 1
        };
        let wait = CHECK_EVERY
            .saturating_mul(1 << check.failures.min(16))
            .min(CHECK_BACKOFF_MAX);
        check.next = Some(Instant::now() + wait);
    });
}

/// Brings the conversation's queued messages up to date with T3 Code's
/// queue and timeline. True when one changed (the phone was told).
pub(crate) async fn check(state: &AppState, conversation: &str) -> Result<bool, CallError> {
    let mut threads = BTreeMap::<String, Vec<Queued>>::new();
    for queued in queued(state, conversation).await {
        threads
            .entry(queued.thread.clone())
            .or_default()
            .push(queued);
    }
    let mut changed = false;
    for (thread, waiting) in threads {
        let gone = |error: &CallError| matches!(error, CallError::Refused { code, .. } if code == "thread_not_found");
        // A T3 thread deleted in T3 Code starts nothing it held.
        let entries = match state.t3_code.queue(&thread).await {
            Err(error) if gone(&error) => Vec::new(),
            entries => entries?,
        };
        let mut left = Vec::new();
        for queued in waiting {
            match entries.iter().find(|entry| entry.run == queued.run) {
                // Edited in T3 Code: Cupola's message has its words, so its
                // agent's record of it is this message (`echoes`).
                Some(entry) if !entry.truncated && entry.text != queued.body => {
                    let hash = hex::encode(Sha256::digest(entry.text.as_bytes()));
                    changed |= state
                        .store
                        .edit_t3_code_queued(
                            conversation,
                            &queued.message,
                            None,
                            &entry.text,
                            &hash,
                        )
                        .await
                        .unwrap_or(false);
                }
                Some(_) => {}
                None => left.push(queued),
            }
        }
        if left.is_empty() {
            continue;
        }
        let sent: Vec<Sent> = left
            .iter()
            .map(|queued| Sent {
                message: queued.message.clone(),
                t3_message: queued.t3_message.clone(),
                attachments: Vec::new(),
            })
            .collect();
        let started = match state
            .t3_code
            .started(&thread, &sent.iter().collect::<Vec<_>>())
            .await
        {
            Err(error) if gone(&error) => Vec::new(),
            started => started?,
        };
        for queued in left {
            let began = started.contains(&queued.message);
            if state
                .store
                .t3_code_queued_left(conversation, &queued.message, None, began)
                .await
                .unwrap_or(false)
            {
                changed = true;
                state
                    .logger
                    .record(
                        "info",
                        "t3_code_queue_left",
                        json!({"conversationId": conversation, "messageId": queued.message,
                        "started": began}),
                    )
                    .ok();
            }
        }
    }
    if changed {
        updated(state, conversation).await;
    }
    Ok(changed)
}

/// The phone's change to a message waiting in T3 Code's queue.
pub(crate) enum Change {
    /// Send now: into the reply running now, which the phone saw as `turn`.
    SendNow(String),
    Edit(String),
    Cancel,
}

/// Applies the phone's change to message `id` when it waits in T3 Code's
/// queue; `None` when it doesn't (Cupola's own queue has it, or nothing).
pub(crate) async fn change(
    state: &AppState,
    conversation: &str,
    id: &str,
    revision: i64,
    change: Change,
) -> Option<Response> {
    let queued = queued(state, conversation)
        .await
        .into_iter()
        .find(|queued| queued.message == id)?;
    let conflict = |text: &'static str| Some((StatusCode::CONFLICT, text).into_response());
    if queued.revision != revision {
        return conflict(
            "This message changed or started. Refresh the queue; your edit has not been applied.",
        );
    }
    let t3 = &state.t3_code;
    let (thread, run) = (queued.thread.as_str(), queued.run.as_str());
    let result = match &change {
        Change::SendNow(turn) => {
            // Into the reply the phone saw running, and only while it is the
            // newest; T3 Code names the run active now.
            if crate::projects::latest_native_turn(state, conversation)
                .await
                .as_deref()
                != Some(turn.as_str())
            {
                return conflict("This message or the active work changed. Nothing was moved.");
            }
            match t3.active_run(thread).await {
                Ok(Some(active)) => {
                    t3.call(
                        "t3_queue_promote_to_steer",
                        json!({"threadId": thread, "queuedRunId": run, "targetRunId": active}),
                    )
                    .await
                }
                Ok(None) => {
                    return conflict("This message or the active work changed. Nothing was moved.")
                }
                Err(error) => Err(error),
            }
        }
        Change::Edit(body) => {
            t3.call(
                "t3_queue_edit",
                json!({"threadId": thread, "queuedRunId": run, "text": body}),
            )
            .await
        }
        Change::Cancel => {
            t3.call(
                "t3_queue_cancel",
                json!({"threadId": thread, "queuedRunId": run}),
            )
            .await
        }
    };
    let _ = state.logger.record(
        "info",
        "t3_code_queue_change",
        json!({"conversationId": conversation, "messageId": id,
            "change": match change { Change::SendNow(_) => "sendNow", Change::Edit(_) => "edit", Change::Cancel => "cancel" },
            "ok": result.is_ok(), "error": result.as_ref().err().map(|error| format!("{error:?}"))}),
    );
    if let Err(error) = result {
        // What T3 Code has now settles it: a run that started, or a change
        // whose answer was lost.
        let _ = check(state, conversation).await;
        return Some(match error {
            CallError::Refused { .. } => (
                StatusCode::CONFLICT,
                "T3 Code didn't take this change: the message may have started there. Refresh the queue.",
            )
                .into_response(),
            CallError::NotConnected | CallError::Unauthorized | CallError::Forbidden => (
                StatusCode::SERVICE_UNAVAILABLE,
                "T3 Code no longer accepts Cupola. Connect T3 Code again in Settings, then try again.",
            )
                .into_response(),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "T3 Code on your computer didn't answer. Refresh the queue and try again.",
            )
                .into_response(),
        });
    }
    let applied = match change {
        Change::SendNow(_) => {
            state
                .store
                .t3_code_queued_left(conversation, id, Some(revision), true)
                .await
        }
        Change::Cancel => {
            state
                .store
                .t3_code_queued_left(conversation, id, Some(revision), false)
                .await
        }
        Change::Edit(body) => {
            let hash = hex::encode(Sha256::digest(body.as_bytes()));
            state
                .store
                .edit_t3_code_queued(conversation, id, Some(revision), &body, &hash)
                .await
        }
    };
    if applied.is_err() {
        return Some(StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    updated(state, conversation).await;
    Some(StatusCode::NO_CONTENT.into_response())
}

/// The phone reads the conversation and its queue again.
async fn updated(state: &AppState, conversation: &str) {
    let _ = crate::publish_event_with_context(
        state,
        crate::WonderEvent::Activity {
            category: "conversation".into(),
            state: "updated".into(),
            detail: None,
        },
        crate::EventContext {
            conversation_id: Some(conversation.to_owned()),
            ..crate::EventContext::default()
        },
    )
    .await;
}

/// One entry of T3 Code's queue (`t3_queue_list`): its run and its words
/// (cut at 1,000 characters when `truncated`).
pub(crate) struct Entry {
    pub(crate) run: String,
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

impl T3Code {
    /// The T3 thread's queue, in delivery order.
    pub(crate) async fn queue(&self, thread: &str) -> Result<Vec<Entry>, CallError> {
        let mut entries = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let mut arguments = json!({"threadId": thread, "limit": 100});
            if !cursor.is_null() {
                arguments["cursor"] = cursor.clone();
            }
            let page = self.call("t3_queue_list", arguments).await?;
            entries.extend(
                page.get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        Some(Entry {
                            run: entry.get("queuedRunId")?.as_str()?.to_owned(),
                            text: entry
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            truncated: entry.get("truncated").and_then(Value::as_bool)
                                == Some(true),
                        })
                    }),
            );
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                return Ok(entries);
            }
        }
        Err(CallError::Protocol("queue too long to read".into()))
    }
}
