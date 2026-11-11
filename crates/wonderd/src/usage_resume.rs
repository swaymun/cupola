//! Resume and snooze usage-limited threads, as T3 Code does: when a Project
//! thread's turn fails because the provider's usage limit was reached, Cupola
//! schedules a continuation for when the limit resets (from the error itself,
//! else the account's exhausted usage windows). The thread list and the
//! conversation show "Resumes at …" with Cancel. The owner sending anything
//! into the thread, turning the setting off, or archiving it cancels the
//! resume. A due resume becomes an ordinary queued owner message, so the
//! shared queue owns ordering, held threads and dispatch admission.

use crate::AppState;
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use std::time::Duration;
use wonder_store::StoredMessage;

/// What a resumed thread is told; it reads like the owner's own "continue".
pub(crate) const CONTINUE: &str = "Continue where you stopped when the usage limit was reached.";
const TICK: Duration = Duration::from_secs(30);
/// A reset is padded so the provider has really reset when the turn starts.
const AFTER_RESET_SECONDS: i64 = 60;
/// A reported reset further away than this is not trusted.
const MAX_WAIT_SECONDS: i64 = 8 * 86_400;
/// The account usage read must not hold up the turn's completion for long.
const USAGE_READ: Duration = Duration::from_secs(3);

/// A scheduled time as the phone reads it.
pub(crate) fn timestamp(at: i64) -> Option<String> {
    Utc.timestamp_opt(at, 0)
        .single()
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Whether a `turn/completed` params ended on a usage limit, and when the
/// error says it resets (Unix seconds): `Some(None)` is a limit without a
/// reset time.
pub(crate) fn usage_limit(params: &Value) -> Option<Option<i64>> {
    let turn = params.get("turn")?;
    if !matches!(turn["status"].as_str(), Some("failed" | "error")) {
        return None;
    }
    let error = &turn["error"];
    // Codex: `codexErrorInfo` is "usageLimitExceeded" or {"usageLimitExceeded": …}.
    let info = &error["codexErrorInfo"];
    let codex =
        info.as_str() == Some("usageLimitExceeded") || info.get("usageLimitExceeded").is_some();
    let message = error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .unwrap_or_default();
    // Claude Code: "Claude AI usage limit reached|<epoch seconds>".
    if let Some((_, epoch)) = message.split_once("usage limit reached|") {
        let digits: String = epoch.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(at) = digits.parse::<i64>() {
            return Some(Some(if at > 100_000_000_000 { at / 1000 } else { at }));
        }
    }
    let lower = message.to_ascii_lowercase();
    let worded = lower.contains("usage limit")
        || lower.contains("hit your limit")
        || lower.contains("rate limit reached");
    (codex || worded).then_some(None)
}

/// Schedules the resume for a turn that `usage_limit` recognised, when the
/// owner has resuming on. A reset already past, unknown or implausibly far
/// away schedules nothing, so the thread shows the failure as before.
pub(crate) async fn note_limited_turn(
    state: &AppState,
    message: &StoredMessage,
    thread: &str,
    turn: &str,
    reported_reset: Option<i64>,
) {
    let Ok(settings) = state.store.continuation_settings().await else {
        return;
    };
    if !settings.resume_after_limit {
        return;
    }
    let Ok(Some(conversation)) = state
        .store
        .project_conversation(&message.conversation_id)
        .await
    else {
        return;
    };
    let lookup = async {
        match reported_reset {
            Some(at) => Some(at),
            None => match crate::claude::for_thread(state, thread).await {
                Ok(rpc) => tokio::time::timeout(
                    USAGE_READ,
                    crate::account_usage::exhausted_until(&rpc, conversation.family),
                )
                .await
                .ok()
                .flatten(),
                Err(_) => None,
            },
        }
    };
    schedule_after_lookup(state, message, turn, lookup).await;
}

/// Schedules the resume once `lookup` finds the reset. Whatever the owner
/// did meanwhile (sending, turning resuming off, archiving the thread) wins:
/// the store rechecks all three at insert.
async fn schedule_after_lookup(
    state: &AppState,
    message: &StoredMessage,
    turn: &str,
    lookup: impl std::future::Future<Output = Option<i64>>,
) {
    let Ok(Some(archive_count)) = state
        .store
        .usage_resume_archive_count(&message.conversation_id)
        .await
    else {
        return;
    };
    let reset = lookup.await;
    let now = Utc::now().timestamp();
    let Some(at) = resume_time(reset, now) else {
        return;
    };
    if !matches!(
        state
            .store
            .schedule_usage_resume(
                &message.conversation_id,
                &message.id,
                turn,
                archive_count,
                at,
                &now_text()
            )
            .await,
        Ok(true)
    ) {
        return;
    }
    let _ = state.logger.record(
        "info",
        "usage_resume_scheduled",
        serde_json::json!({"conversationId": message.conversation_id, "resumeAt": at}),
    );
}

fn resume_time(reset: Option<i64>, now: i64) -> Option<i64> {
    let reset = reset?;
    (reset > now - AFTER_RESET_SECONDS && reset - now <= MAX_WAIT_SECONDS)
        .then_some(reset.max(now) + AFTER_RESET_SECONDS)
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        loop {
            interval.tick().await;
            tick(&state, Utc::now().timestamp()).await;
        }
    })
}

/// Queues the continuation of every resume due at `now`; returns the
/// conversations resumed.
pub(crate) async fn tick(state: &AppState, now: i64) -> Vec<String> {
    let mut resumed = Vec::new();
    let Ok(due) = state.store.due_usage_resumes(now).await else {
        return resumed;
    };
    if due.is_empty() {
        return resumed;
    }
    // An update in progress owns the host; the next tick catches up.
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return resumed;
    };
    let Ok(Some(device)) = state.store.automation_message_device_id().await else {
        return resumed;
    };
    for conversation in due {
        if let Ok(Some(_)) = state
            .store
            .materialize_usage_resume(&conversation, &device, CONTINUE, now, &now_text())
            .await
        {
            resumed.push(conversation);
        }
    }
    if !resumed.is_empty() {
        state.dispatch_wake.notify_one();
    }
    resumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failed(error: Value) -> Value {
        json!({"turn": {"id": "t", "status": "failed", "error": error}})
    }

    #[test]
    fn recognises_codex_and_claude_usage_limits_and_their_reset() {
        assert_eq!(
            usage_limit(&failed(
                json!({"message": "Claude AI usage limit reached|1760000000"})
            )),
            Some(Some(1_760_000_000))
        );
        assert_eq!(
            usage_limit(&failed(
                json!({"message": "x", "codexErrorInfo": "usageLimitExceeded"})
            )),
            Some(None)
        );
        assert_eq!(
            usage_limit(&failed(
                json!({"message": "x", "codexErrorInfo": {"usageLimitExceeded": {}}})
            )),
            Some(None)
        );
        assert_eq!(
            usage_limit(&failed(
                json!({"message": "You've hit your usage limit. Try again later."})
            )),
            Some(None)
        );
        // Other failures, and a completed turn, are not limits.
        assert_eq!(
            usage_limit(&failed(json!({"message": "Network error"}))),
            None
        );
        assert_eq!(
            usage_limit(
                &json!({"turn": {"status": "completed", "error": {"message": "usage limit reached|1"}}})
            ),
            None
        );
    }

    #[test]
    fn resumes_just_after_a_plausible_reset() {
        let now = 1_000_000;
        assert_eq!(resume_time(Some(now + 600), now), Some(now + 660));
        // A reset that just passed resumes soon; one long past or unknown, never.
        assert_eq!(resume_time(Some(now - 10), now), Some(now + 60));
        assert_eq!(resume_time(Some(now - 3_600), now), None);
        assert_eq!(resume_time(None, now), None);
        assert_eq!(resume_time(Some(now + 9 * 86_400), now), None);
    }
}

#[cfg(all(test, unix))]
mod flow_tests {
    use super::*;
    use serde_json::json;
    use wonder_store::{AgentFamily, MessageInsert};

    const CONVERSATION: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";

    async fn limited_thread() -> (tempfile::TempDir, AppState, i64) {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        crate::test_support::bind_project_thread(
            &state,
            CONVERSATION,
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                "first",
                "work",
                "hash",
                CONVERSATION,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("message");
        };
        state
            .store
            .update_message_delivery(&message.id, "accepted_by_codex", Some("thread"), Some("t1"))
            .await
            .unwrap();
        let reset = Utc::now().timestamp() + 3_600;
        assert!(crate::process_app_server_notification(&state, json!({"method": "turn/completed",
            "params": {"threadId": "thread", "turnId": "t1", "turn": {"id": "t1", "status": "failed",
                "error": {"message": format!("Claude AI usage limit reached|{reset}")}}}}), false).await);
        (dir, state, reset)
    }

    #[tokio::test]
    async fn a_limited_turn_resumes_once_after_the_reset_as_a_queued_continue() {
        let (_dir, state, reset) = limited_thread().await;
        let resumes = state.store.usage_resumes().await.unwrap();
        assert_eq!(resumes[CONVERSATION], reset + AFTER_RESET_SECONDS);
        let conversation = state
            .store
            .project_conversation(CONVERSATION)
            .await
            .unwrap()
            .unwrap();
        let detail = serde_json::to_value(
            crate::projects::conversation_detail(&state, &conversation)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            detail["resumesAt"],
            json!(timestamp(reset + AFTER_RESET_SECONDS))
        );
        // Nothing happens before the reset.
        assert!(tick(&state, reset).await.is_empty());
        let resumed = tick(&state, reset + AFTER_RESET_SECONDS).await;
        assert_eq!(resumed, vec![CONVERSATION.to_owned()]);
        let queued = state
            .store
            .message_by_id(&format!("usage-resume:{CONVERSATION}:t1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (queued.body.as_str(), queued.state.as_str()),
            (CONTINUE, "accepted_by_wonder")
        );
        // Consumed: a later tick sends nothing more.
        assert!(tick(&state, reset + 7_200).await.is_empty());
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_codex_limit_without_a_time_resumes_when_the_account_window_resets() {
        let (dir, state, _) = limited_thread().await;
        state.store.cancel_usage_resume(CONVERSATION).await.unwrap();
        let reset = Utc::now().timestamp() + 7_200;
        std::fs::write(
            dir.path().join("usage-fixture.json"),
            json!({"rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": reset},
                "secondary": {"usedPercent": 30, "windowDurationMins": 10080, "resetsAt": reset + 86_400}
            }})
            .to_string(),
        )
        .unwrap();
        let rpc = state.projects.codex.lock().await.rpc();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, rpc.health());
        let message = state
            .store
            .messages_for_conversation(CONVERSATION)
            .await
            .unwrap()
            .remove(0);
        let params = json!({"turn": {"id": "t2", "status": "failed",
            "error": {"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"}}});
        let limit = usage_limit(&params).expect("a usage limit");
        note_limited_turn(&state, &message, "thread", "t2", limit).await;
        assert_eq!(
            state.store.usage_resumes().await.unwrap().get(CONVERSATION),
            Some(&(reset + AFTER_RESET_SECONDS))
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_and_the_setting_stop_a_resume() {
        let (_dir, state, reset) = limited_thread().await;
        assert!(state.store.cancel_usage_resume(CONVERSATION).await.unwrap());
        assert!(tick(&state, reset + 7_200).await.is_empty());
        // With resuming off, a limited turn schedules nothing.
        state
            .store
            .set_continuation_settings(Some(false), None)
            .await
            .unwrap();
        let message = state
            .store
            .messages_for_conversation(CONVERSATION)
            .await
            .unwrap()
            .remove(0);
        note_limited_turn(&state, &message, "thread", "t2", Some(reset)).await;
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_owner_send_before_the_schedule_lands_wins() {
        let (_dir, state, reset) = limited_thread().await;
        state.store.cancel_usage_resume(CONVERSATION).await.unwrap();
        let message = state
            .store
            .messages_for_conversation(CONVERSATION)
            .await
            .unwrap()
            .remove(0);
        // The owner replies while the limited turn's reset is being read.
        state
            .store
            .insert_dispatch_message(
                "owner",
                "second",
                "keep going",
                "hash2",
                CONVERSATION,
                &[],
                "later",
                true,
            )
            .await
            .unwrap();
        note_limited_turn(&state, &message, "thread", "t1", Some(reset)).await;
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn archiving_then_restoring_a_thread_does_not_bring_its_resume_back() {
        let (_dir, state, reset) = limited_thread().await;
        assert!(state
            .store
            .archive_project_conversation(CONVERSATION, &[], "later")
            .await
            .unwrap()
            .is_some());
        assert!(state
            .store
            .set_project_conversation_wonder_archived(CONVERSATION, false, "later")
            .await
            .unwrap());
        assert!(tick(&state, reset + AFTER_RESET_SECONDS).await.is_empty());
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
        // The other archive paths drop it in the same step too.
        for auto in [false, true] {
            let (_dir, state, reset) = limited_thread().await;
            if auto {
                let conversation = state
                    .store
                    .project_conversation(CONVERSATION)
                    .await
                    .unwrap()
                    .unwrap();
                let last_sent = state
                    .store
                    .messages_for_conversation(CONVERSATION)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|message| message.created_at)
                    .max();
                // The limited send settled as failed; nothing holds the thread.
                settle_limited_send(&state).await;
                assert!(state
                    .store
                    .auto_archive_project_conversation(
                        CONVERSATION,
                        &conversation.last_activity_at,
                        last_sent.as_deref(),
                        &[],
                        "later",
                    )
                    .await
                    .unwrap());
            } else {
                assert!(state
                    .store
                    .set_project_conversation_wonder_archived(CONVERSATION, true, "later")
                    .await
                    .unwrap());
            }
            state
                .store
                .set_project_conversation_wonder_archived(CONVERSATION, false, "later")
                .await
                .unwrap();
            assert!(tick(&state, reset + AFTER_RESET_SECONDS).await.is_empty());
        }
    }

    #[tokio::test]
    async fn archiving_during_the_reset_lookup_then_restoring_sends_nothing() {
        let (_dir, state, reset) = limited_thread().await;
        state.store.cancel_usage_resume(CONVERSATION).await.unwrap();
        let message = state
            .store
            .messages_for_conversation(CONVERSATION)
            .await
            .unwrap()
            .remove(0);
        // The owner archives the thread while its reset is being read.
        let lookup = async {
            assert!(state
                .store
                .set_project_conversation_wonder_archived(CONVERSATION, true, "later")
                .await
                .unwrap());
            Some(reset)
        };
        schedule_after_lookup(&state, &message, "t1", lookup).await;
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
        state
            .store
            .set_project_conversation_wonder_archived(CONVERSATION, false, "later")
            .await
            .unwrap();
        assert!(tick(&state, reset + AFTER_RESET_SECONDS).await.is_empty());
        assert!(state
            .store
            .message_by_id(&format!("usage-resume:{CONVERSATION}:t1"))
            .await
            .unwrap()
            .is_none());
    }

    async fn settle_limited_send(state: &AppState) {
        let message = state
            .store
            .messages_for_conversation(CONVERSATION)
            .await
            .unwrap()
            .remove(0);
        state
            .store
            .update_message_delivery(&message.id, "failed", Some("thread"), Some("t1"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_owner_send_committed_before_its_cancel_still_stops_the_resume() {
        let (_dir, state, reset) = limited_thread().await;
        // The owner's message is in the store, but send_message has not yet
        // cancelled the resume when the due tick runs.
        state
            .store
            .insert_dispatch_message(
                "owner",
                "second",
                "keep going",
                "hash2",
                CONVERSATION,
                &[],
                "later",
                true,
            )
            .await
            .unwrap();
        assert!(tick(&state, reset + AFTER_RESET_SECONDS).await.is_empty());
        assert!(state
            .store
            .message_by_id(&format!("usage-resume:{CONVERSATION}:t1"))
            .await
            .unwrap()
            .is_none());
        assert!(state.store.usage_resumes().await.unwrap().is_empty());
    }
}
