//! Threads T3 Code owns: a Cupola thread T3 Code started or knows goes
//! through T3 Code whenever T3 Code runs (wonderd's `t3_code`), so which
//! T3 thread it is outlives a restart while T3 Code is closed.

use crate::Store;
use sqlx::Row;

/// `messages` row `id` is held in T3 Code's queue: delivered to T3 Code
/// (`completed`) with `t3QueuedRunId` in the intent it was last sent with.
/// A macro so queries stay static strings.
macro_rules! t3_queued {
    () => {
        "state='completed' AND EXISTS(SELECT 1 FROM dispatch_attempts d WHERE d.id=(SELECT MAX(id) FROM dispatch_attempts WHERE message_id=messages.id) AND json_extract(d.context_json,'$.deliveryMethod')='t3_thread_send' AND json_type(d.context_json,'$.t3QueuedRunId')='text')"
    };
}

/// Persistent, invalidatable echo projection. Revision fences a concurrent
/// edit/reset from publishing a projection built from stale history.
#[derive(Debug, sqlx::FromRow)]
pub struct T3EchoCache {
    pub revision: i64,
    pub signature: Option<String>,
    pub last_sequence: i64,
    pub matches_json: String,
}

impl Store {
    pub async fn t3_echo_cache(&self, conversation: &str) -> Result<T3EchoCache, sqlx::Error> {
        let load = || {
            sqlx::query_as::<_,T3EchoCache>("SELECT revision,signature,last_sequence,matches_json FROM t3_echo_cache WHERE conversation_id=?").bind(conversation)
        };
        if let Some(cache) = load().fetch_optional(&self.pool).await? {
            return Ok(cache);
        }
        sqlx::query("INSERT OR IGNORE INTO t3_echo_cache(conversation_id) VALUES(?)")
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        load().fetch_one(&self.pool).await
    }

    pub async fn save_t3_echo_cache(
        &self,
        conversation: &str,
        revision: i64,
        signature: &str,
        last_sequence: i64,
        matches_json: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE t3_echo_cache SET signature=?,last_sequence=?,matches_json=? WHERE conversation_id=? AND revision=? AND (signature IS NOT ? OR last_sequence<=?)")
            .bind(signature).bind(last_sequence).bind(matches_json).bind(conversation).bind(revision)
            .bind(signature).bind(last_sequence).execute(&self.pool).await?;
        Ok(())
    }

    /// The T3 thread this conversation's native session is, when T3 Code
    /// owns it.
    pub async fn t3_code_thread(
        &self,
        conversation: &str,
        native_session: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT t3_thread_id FROM t3_code_threads WHERE conversation_id=? AND native_session_id=?",
        )
        .bind(conversation)
        .bind(native_session)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.get("t3_thread_id")))
    }

    /// The T3 thread that is the whole of an OpenCode or Cursor conversation
    /// (no native session; recorded as `t3:<thread>`), while its runtime
    /// binding still names it.
    pub async fn t3_native_thread(
        &self,
        conversation: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT t.t3_thread_id FROM t3_code_threads t JOIN runtime_bindings r ON r.conversation_id=t.conversation_id AND r.runtime_thread_id IN ('opencode-t3:' || t.t3_thread_id, 'cursor-t3:' || t.t3_thread_id) WHERE t.conversation_id=? AND t.native_session_id = 't3:' || t.t3_thread_id",
        )
        .bind(conversation)
        .fetch_optional(&self.pool)
        .await
    }

    /// Remembers that T3 Code owns this conversation's native session as
    /// `t3_thread`.
    pub async fn set_t3_code_thread(
        &self,
        conversation: &str,
        native_session: &str,
        t3_thread: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO t3_code_threads(conversation_id,native_session_id,t3_thread_id,updated_at) VALUES(?,?,?,?) ON CONFLICT(conversation_id) DO UPDATE SET native_session_id=excluded.native_session_id, t3_thread_id=excluded.t3_thread_id, updated_at=excluded.updated_at")
            .bind(conversation)
            .bind(native_session)
            .bind(t3_thread)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The one conversation whose current native session T3 Code runs as
    /// `t3_thread`: where that T3 thread's questions belong on the phone.
    /// A session reset or switched since is no longer that thread.
    pub async fn t3_code_conversation(
        &self,
        t3_thread: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let owners: Vec<String> = sqlx::query_scalar(
            "SELECT t.conversation_id FROM t3_code_threads t JOIN project_conversations p ON p.conversation_id=t.conversation_id AND (p.native_session_id=t.native_session_id OR (t.native_session_id = 't3:' || t.t3_thread_id AND EXISTS (SELECT 1 FROM runtime_bindings r WHERE r.conversation_id=t.conversation_id AND r.runtime_thread_id IN ('opencode-t3:' || t.t3_thread_id, 'cursor-t3:' || t.t3_thread_id)))) WHERE t.t3_thread_id=? LIMIT 2",
        )
        .bind(t3_thread)
        .fetch_all(&self.pool)
        .await?;
        Ok(match owners.as_slice() {
            [owner] => Some(owner.clone()),
            _ => None,
        })
    }

    /// T3 Code no longer has the conversation's T3 thread (deleted there).
    pub async fn forget_t3_code_thread(&self, conversation: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM t3_code_threads WHERE conversation_id=?")
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The conversation's messages last sent with `t3_thread_send`, in the
    /// order they were sent.
    pub async fn t3_code_sent_messages(
        &self,
        conversation: &str,
    ) -> Result<Vec<T3CodeSend>, sqlx::Error> {
        sqlx::query_as("SELECT m.id, m.client_message_id, m.body, m.state, m.queue_revision AS revision, d.context_json AS intent FROM messages m JOIN dispatch_attempts d ON d.id = (SELECT MAX(id) FROM dispatch_attempts WHERE message_id = m.id) WHERE m.conversation_id = ? AND json_extract(d.context_json, '$.deliveryMethod') = 't3_thread_send' ORDER BY d.id")
            .bind(conversation)
            .fetch_all(&self.pool)
            .await
    }

    /// The first message of a thread Start in T3 Code launched, as
    /// delivered there (`completed`): its intent (`context`, a
    /// `t3_thread_send` one) pairs it with its agent's record once the
    /// session shows. Saved once per (device, client message); it is never
    /// dispatched. Returns the message's ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_t3_code_launch_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        t3_thread: &str,
        context: &serde_json::Value,
        now: &str,
    ) -> Result<String, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM messages WHERE device_id=? AND client_message_id=?")
                .bind(device_id)
                .bind(client_message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(id) = existing {
            tx.commit().await?;
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO messages (id, device_id, client_message_id, body, body_sha256, conversation_id, state, codex_thread_id, created_at) VALUES (?, ?, ?, ?, ?, ?, 'completed', ?, ?)")
            .bind(&id)
            .bind(device_id)
            .bind(client_message_id)
            .bind(body)
            .bind(body_sha256)
            .bind(conversation_id)
            .bind(t3_thread)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO dispatch_attempts (message_id, phase, thread_id, context_json) VALUES (?, 'submitting', ?, ?)")
            .bind(&id)
            .bind(t3_thread)
            .bind(context.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Merges `patch` (a JSON object; a null field is removed) into the
    /// intent a message was last sent to T3 Code with: T3 Code's ID for it,
    /// and what Cupola learned from T3 Code's timeline about it.
    pub async fn update_t3_code_intent(
        &self,
        message: &str,
        patch: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE dispatch_attempts SET context_json = json_patch(context_json, ?) WHERE id = (SELECT MAX(id) FROM dispatch_attempts WHERE message_id = ?) AND json_extract(context_json, '$.deliveryMethod') = 't3_thread_send'")
            .bind(patch.to_string())
            .bind(message)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Changes the words of a message T3 Code holds in its queue
    /// (`t3QueuedRunId` in its intent), as T3 Code now has them: edited
    /// from the phone or in T3 Code. With `revision`, only while the
    /// message is still that revision. True when it changed.
    pub async fn edit_t3_code_queued(
        &self,
        conversation: &str,
        message: &str,
        revision: Option<i64>,
        body: &str,
        body_sha256: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(concat!("UPDATE messages SET body=?, body_sha256=?, queue_revision=queue_revision+1 WHERE id=? AND conversation_id=? AND (? IS NULL OR queue_revision=?) AND body<>? AND ", t3_queued!()))
            .bind(body)
            .bind(body_sha256)
            .bind(message)
            .bind(conversation)
            .bind(revision)
            .bind(revision)
            .bind(body)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// A message T3 Code held in its queue left it: T3 Code `started` it
    /// (its run began, or it steered the running one), and it takes its
    /// place in history now, as any queued message does when its turn
    /// starts; or it was cancelled (from the phone, or in T3 Code), as a
    /// message cancelled from Cupola's own queue is (`interrupted`). With
    /// `revision`, only while the message is still that revision. True when
    /// it changed.
    pub async fn t3_code_queued_left(
        &self,
        conversation: &str,
        message: &str,
        revision: Option<i64>,
        started: bool,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(concat!("UPDATE messages SET state=CASE WHEN ? THEN state ELSE 'interrupted' END, queue_revision=queue_revision+1 WHERE id=? AND conversation_id=? AND (? IS NULL OR queue_revision=?) AND ", t3_queued!()))
            .bind(started)
            .bind(message)
            .bind(conversation)
            .bind(revision)
            .bind(revision)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if changed {
            let patch = if started {
                serde_json::json!({"t3QueuedRunId": null, "t3Started": true, "t3Failures": null, "t3RetryAtMs": null})
            } else {
                serde_json::json!({"t3QueuedRunId": null})
            };
            sqlx::query("UPDATE dispatch_attempts SET context_json = json_patch(context_json, ?) WHERE id = (SELECT MAX(id) FROM dispatch_attempts WHERE message_id = ?)")
                .bind(patch.to_string())
                .bind(message)
                .execute(&mut *tx)
                .await?;
            if started {
                sqlx::query("UPDATE history_entries SET sort_ms = MAX(sort_ms, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)) WHERE source = 'message' AND source_id = ?")
                    .bind(message)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// The user messages an agent recorded itself in the conversation's
    /// saved history after row `after`, oldest first: those without a Cupola
    /// message's client ID (typed on the Mac, or sent through T3 Code).
    /// Each is (history row, turn ID, item ID, the item as saved).
    pub async fn t3_code_user_records(
        &self,
        conversation: &str,
        after: i64,
    ) -> Result<Vec<(i64, String, String, String)>, sqlx::Error> {
        // `instr` skips most rows without parsing their JSON.
        sqlx::query_as("SELECT sequence, json_extract(detail,'$.turnId'), json_extract(detail,'$.itemId'), json_extract(detail,'$.item') FROM (SELECT sequence, json_extract(payload_json,'$.event.data.detail') AS detail FROM history_entries INDEXED BY history_t3_user_sequence WHERE conversation_id=? AND source='event' AND hidden_by_reset=0 AND sequence>? AND instr(payload_json,'userMessage')>0 AND json_type(payload_json,'$.event.data.detail')='text' AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.type')='userMessage' AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.clientId') IS NULL) WHERE json_extract(detail,'$.item.type')='userMessage' AND json_extract(detail,'$.item.clientId') IS NULL AND json_type(detail,'$.turnId')='text' AND json_type(detail,'$.itemId')='text' AND json_type(detail,'$.item')='object' ORDER BY sequence")
            .bind(conversation)
            .bind(after)
            .fetch_all(&self.pool)
            .await
    }
}

/// A message last sent with `t3_thread_send`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct T3CodeSend {
    pub id: String,
    pub client_message_id: String,
    pub body: String,
    /// Its delivery state.
    pub state: String,
    /// Its queue revision: an edit or cancel names the one it saw.
    pub revision: i64,
    /// The intent it was sent with (JSON).
    pub intent: String,
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[tokio::test]
    async fn t3_native_ownership_matches_literal_runtime_identity() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let thread = "thread:delegated-task:command%3Amcp%3Afixture_with_underscores";
        crate::seed_project_thread(&store, "seed", "Chat", "/work", None, None, "now")
            .await
            .unwrap();
        for family in [crate::AgentFamily::OpenCode, crate::AgentFamily::Cursor] {
            let chat = family.as_str();
            store
                .create_project_conversation(crate::ProjectConversationInsert {
                    conversation_id: chat,
                    project_id: "project-seed",
                    family,
                    provider_store: "fixture-store",
                    native_session_id: None,
                    cwd: "/work",
                    roots_revision: 1,
                    title: "Chat",
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
            let runtime = format!("{}t3:{thread}", family.provider().thread_id_prefix.unwrap());
            store
                .bind_project_runtime(chat, family, "fixture-store", &runtime, None, "now")
                .await
                .unwrap();
            store
                .set_t3_code_thread(chat, &format!("t3:{thread}"), thread, "now")
                .await
                .unwrap();
            assert_eq!(
                store.t3_native_thread(chat).await.unwrap().as_deref(),
                Some(thread)
            );
            assert_eq!(
                store.t3_code_conversation(thread).await.unwrap().as_deref(),
                Some(chat)
            );
            // `%` and `_` are literal identity, not SQL patterns.
            let lookalike = runtime.replace('%', "wildcard").replace('_', "x");
            sqlx::query("UPDATE runtime_bindings SET runtime_thread_id=? WHERE conversation_id=?")
                .bind(lookalike)
                .bind(chat)
                .execute(&store.pool)
                .await
                .unwrap();
            assert!(store.t3_native_thread(chat).await.unwrap().is_none());
            assert!(store.t3_code_conversation(thread).await.unwrap().is_none());
            sqlx::query("UPDATE runtime_bindings SET runtime_thread_id=? WHERE conversation_id=?")
                .bind(&runtime)
                .bind(chat)
                .execute(&store.pool)
                .await
                .unwrap();
            store
                .set_t3_code_thread(chat, "t3:stale", thread, "now")
                .await
                .unwrap();
            assert!(store.t3_native_thread(chat).await.unwrap().is_none());
            assert!(store.t3_code_conversation(thread).await.unwrap().is_none());
            store.forget_t3_code_thread(chat).await.unwrap();
        }
    }

    // Contract: T3 Code's ownership of a thread survives a reopened store
    // and applies only to the native session it was matched with.
    #[tokio::test]
    async fn echo_cache_survives_reopen_and_rejects_a_stale_edit_projection() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        crate::seed_project_thread(&store, "chat", "Chat", "/work", None, None, "now")
            .await
            .unwrap();
        let original = store.t3_echo_cache("chat").await.unwrap();
        sqlx::query("INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json) VALUES('chat','event','test-user',1,?)")
            .bind(serde_json::json!({"event":{"data":{"detail":serde_json::json!({"item":{"type":"userMessage"}}).to_string()}}}).to_string()).execute(&store.pool).await.unwrap();
        store
            .save_t3_echo_cache("chat", original.revision, "signature", 100, "[[1,2,3]]")
            .await
            .unwrap();
        drop(store);
        let store = Store::connect(&url).await.unwrap();
        let cached = store.t3_echo_cache("chat").await.unwrap();
        assert_eq!(cached.signature.as_deref(), Some("signature"));
        assert_eq!(cached.last_sequence, 100);
        // Tool output quoting the word userMessage is not a user-row edit.
        sqlx::query("INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json) VALUES('chat','event','tool',1,?)")
            .bind(serde_json::json!({"event":{"data":{"detail":serde_json::json!({"item":{"type":"commandExecution","output":"userMessage"}}).to_string()}}}).to_string())
            .execute(&store.pool).await.unwrap();
        sqlx::query("UPDATE history_entries SET payload_json=replace(payload_json,'userMessage','userMessage changed') WHERE source_id='tool'")
            .execute(&store.pool).await.unwrap();
        assert_eq!(
            store.t3_echo_cache("chat").await.unwrap().revision,
            cached.revision
        );
        for mutation in [
            "UPDATE history_entries SET hidden_by_reset=1 WHERE source_id='test-user'",
            "UPDATE history_entries SET payload_json='{}' WHERE source_id='test-user'",
        ] {
            sqlx::query(mutation).execute(&store.pool).await.unwrap();
            store
                .save_t3_echo_cache("chat", cached.revision, "stale", 101, "[]")
                .await
                .unwrap();
            let reset = store.t3_echo_cache("chat").await.unwrap();
            assert!(reset.revision > cached.revision);
            assert!(reset.signature.is_none());
        }
    }

    #[tokio::test]
    async fn t3_code_ownership_outlives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let t = "2026-10-21T10:00:00.000Z";
        let conversation = "chat";
        crate::seed_project_thread(&store, conversation, "Chat", "/work", None, None, t)
            .await
            .unwrap();
        store
            .set_t3_code_thread(conversation, "session", "t3-thread", t)
            .await
            .unwrap();
        drop(store);

        let store = Store::connect(&url).await.unwrap();
        assert_eq!(
            store.t3_code_thread(conversation, "session").await.unwrap(),
            Some("t3-thread".into())
        );
        assert_eq!(
            store.t3_code_thread(conversation, "reset").await.unwrap(),
            None,
            "another native session"
        );
        store.forget_t3_code_thread(conversation).await.unwrap();
        assert_eq!(
            store.t3_code_thread(conversation, "session").await.unwrap(),
            None
        );
    }

    // Contract: a question T3 Code asks in its thread reaches the one Project
    // conversation whose current session that T3 thread is; never one whose
    // session was reset since, and never a guess between two.
    #[tokio::test]
    async fn a_t3_threads_question_routes_to_the_conversation_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let t = "2026-10-22T10:00:00.000Z";
        for chat in ["chat", "other"] {
            crate::bind_project_fixture(
                &store,
                chat,
                crate::AgentFamily::Codex,
                &format!("{chat}-session"),
                None,
                t,
            )
            .await
            .unwrap();
        }
        let route = |thread: &'static str| {
            let store = &store;
            async move {
                store
                    .approval_push_conversation(thread, "run")
                    .await
                    .unwrap()
            }
        };
        assert_eq!(route("t3-thread").await, None, "T3 Code owns no thread yet");
        store
            .set_t3_code_thread("chat", "chat-session", "t3-thread", t)
            .await
            .unwrap();
        assert_eq!(route("t3-thread").await, Some("chat".into()));
        assert_eq!(route("t3-elsewhere").await, None);
        // Two conversations claiming one T3 thread: neither gets its questions.
        store
            .set_t3_code_thread("other", "other-session", "t3-thread", t)
            .await
            .unwrap();
        assert_eq!(route("t3-thread").await, None);
        store.forget_t3_code_thread("other").await.unwrap();
        // Matched with a session the conversation no longer runs.
        store
            .set_t3_code_thread("chat", "reset-session", "t3-thread", t)
            .await
            .unwrap();
        assert_eq!(route("t3-thread").await, None);
    }
}
