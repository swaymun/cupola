//! Async prompts are messages, not pending App Server RPCs.
use super::*;

#[derive(Debug)]
pub struct AsyncQuestion {
    pub id: String,
    pub conversation_id: String,
    pub runtime_conversation_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub questions: serde_json::Value,
    pub response: Option<serde_json::Value>,
    pub state: String,
    pub expires_at_ms: i64,
}
// A question stays in its persisted conversation; child continuations resolve a verified parent.
macro_rules! scoped_questions {
    ($query:literal) => { concat!("WITH scoped_questions AS (SELECT q.*, q.rowid AS position, q.conversation_id AS visible_conversation FROM async_questions q) ", $query) };
}
pub(crate) use scoped_questions;

// The one Project-membership rule: a thread belongs when its folder is a
// saved Project folder exactly (never a prefix or subfolder) or a linked
// worktree wonderd verified for that folder (`project_worktrees`) while
// worktree chats are on. Child tasks inherit their verified parent's Project.
macro_rules! verified_subagent_root_sql {
    () => { r#"
WITH RECURSIVE parents(id,depth) AS (
 SELECT ?,0
 UNION ALL
 SELECT s.parent_conversation_id,p.depth+1 FROM parents p
 JOIN subagent_ownership s ON s.conversation_id=p.id
 JOIN conversations child ON child.id=s.conversation_id AND child.codex_thread_id=s.thread_id
 JOIN conversations parent ON parent.id=s.parent_conversation_id AND parent.codex_thread_id=s.parent_thread_id
 JOIN runtime_bindings cb ON cb.conversation_id=child.id AND cb.runtime_thread_id=s.thread_id
 JOIN runtime_bindings pb ON pb.conversation_id=parent.id AND pb.runtime_thread_id=s.parent_thread_id
   AND pb.agent_family=cb.agent_family AND pb.provider_store=cb.provider_store
 WHERE p.depth<32 AND s.is_archived=0
)
SELECT pc.conversation_id FROM parents ancestor
JOIN project_conversations pc ON pc.conversation_id=ancestor.id
JOIN projects p ON p.id=pc.project_id AND p.is_included=1
WHERE NOT EXISTS(SELECT 1 FROM subagent_ownership s WHERE s.conversation_id=ancestor.id)
 AND (ancestor.depth=0 OR EXISTS(SELECT 1 FROM runtime_bindings rb JOIN conversations c ON c.id=rb.conversation_id AND c.codex_thread_id=rb.runtime_thread_id WHERE rb.conversation_id=pc.conversation_id AND rb.agent_family=pc.agent_family AND rb.provider_store=pc.provider_store))
 AND EXISTS(SELECT 1 FROM project_roots r WHERE r.project_id=p.id AND
   (pc.cwd=r.canonical_path OR pc.cwd=r.path OR (
    COALESCE((SELECT include_worktrees FROM project_settings WHERE id=1),1)=1 AND
    EXISTS(SELECT 1 FROM project_worktrees w WHERE w.root_path=r.canonical_path AND w.path=pc.cwd))))
ORDER BY ancestor.depth DESC LIMIT 1
"# };
}
pub(crate) use verified_subagent_root_sql;
pub(crate) const VERIFIED_SUBAGENT_ROOT_SQL: &str = verified_subagent_root_sql!();

impl Store {
    pub const QUESTION_PARENT_UNAVAILABLE: &str =
        "The parent conversation is unavailable. Your answer is still saved as a draft.";
    pub async fn save_async_question(
        &self,
        conversation: &str,
        thread: &str,
        turn: &str,
        item: &str,
        questions: &str,
        deadline: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO async_questions(id,conversation_id,thread_id,turn_id,item_id,questions_json,expires_at_ms) VALUES (?,?,?,?,?,?,?) ON CONFLICT(thread_id,turn_id,item_id) DO NOTHING")
            .bind(uuid::Uuid::new_v4().to_string()).bind(conversation).bind(thread).bind(turn).bind(item).bind(questions).bind(deadline).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn async_questions(
        &self,
        conversation: &str,
        now: i64,
    ) -> Result<Vec<AsyncQuestion>, sqlx::Error> {
        let rows = sqlx::query(scoped_questions!(
            " SELECT * FROM scoped_questions WHERE visible_conversation=? ORDER BY position DESC LIMIT 100"
        ))
        .bind(conversation)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Self::async_question(&r, now))
            .collect()
    }
    /// Questions still waiting for an answer in any conversation, newest
    /// first, so a phone can find them without opening each chat.
    pub async fn pending_async_questions(
        &self,
        now: i64,
    ) -> Result<Vec<AsyncQuestion>, sqlx::Error> {
        let rows = sqlx::query(scoped_questions!(
            " SELECT * FROM scoped_questions WHERE state='pending' AND expires_at_ms>? ORDER BY position DESC LIMIT 200"
        ))
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Self::async_question(&r, now))
            .collect()
    }
    fn async_question(r: &sqlx::sqlite::SqliteRow, now: i64) -> Result<AsyncQuestion, sqlx::Error> {
        let deadline = r.get("expires_at_ms");
        let state: String = r.get("state");
        Ok(AsyncQuestion {
            id: r.get("id"),
            conversation_id: r.get("visible_conversation"),
            runtime_conversation_id: r.get("conversation_id"),
            turn_id: r.get("turn_id"),
            item_id: r.get("item_id"),
            questions: serde_json::from_str(r.get("questions_json"))
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            state: if state == "pending" && now >= deadline {
                "expired".into()
            } else {
                state
            },
            response: r
                .get::<Option<String>, _>("response_json")
                .and_then(|v| serde_json::from_str(&v).ok()),
            expires_at_ms: deadline,
        })
    }
    pub async fn question_answer_messages(
        &self,
        conversation: &str,
    ) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.* FROM messages m JOIN async_questions q ON q.message_id=m.id WHERE m.conversation_id=?")
            .bind(conversation).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(stored_message).collect())
    }
    /// One SQLite commit owns both the reply and its execution intent. A second
    /// device cannot answer or skip a question that already has a winner.
    pub async fn answer_async_question(
        &self,
        conversation: &str,
        id: &str,
        device: &str,
        response: &str,
        body: Option<&str>,
        now: i64,
    ) -> Result<bool, sqlx::Error> {
        use sha2::{Digest, Sha256};
        let mut tx = self.pool.begin().await?;
        let state = if body.is_some() {
            "answered"
        } else {
            "dismissed"
        };
        let changed = sqlx::query("UPDATE async_questions SET state=?,response_json=? WHERE id=? AND conversation_id=? AND state='pending' AND expires_at_ms>?")
            .bind(state).bind(response).bind(id).bind(conversation).bind(now).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            let saved: Option<String> = sqlx::query_scalar(scoped_questions!(
                " SELECT response_json FROM scoped_questions WHERE id=? AND visible_conversation=?"
            ))
            .bind(id)
            .bind(conversation)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            if saved.as_deref() == Some(response) {
                return Ok(true);
            }
            return Ok(false);
        }
        if let Some(body) = body {
            let row = sqlx::query(
                "SELECT conversation_id,thread_id,turn_id FROM async_questions WHERE id=?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let runtime_conversation: String = row.get("conversation_id");
            let thread: String = row.get("thread_id");
            let turn: String = row.get("turn_id");
            // Keep the answer on its exact child question, but continue work in
            // the verified parent. Direct child sends/Guide are intentionally
            // forbidden, including durable work left by older clients.
            let child: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM subagent_ownership WHERE conversation_id=?)",
            )
            .bind(&runtime_conversation)
            .fetch_one(&mut *tx)
            .await?;
            let parent: Option<String> = if child {
                let root: Option<String> = sqlx::query_scalar(VERIFIED_SUBAGENT_ROOT_SQL)
                    .bind(&runtime_conversation)
                    .fetch_optional(&mut *tx)
                    .await?;
                let root = root.ok_or_else(|| {
                    sqlx::Error::Protocol(Self::QUESTION_PARENT_UNAVAILABLE.into())
                })?;
                Some(root)
            } else {
                None
            };
            let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND codex_thread_id=? AND codex_turn_id=? AND state IN ('accepted_by_codex','streaming'))")
                .bind(&runtime_conversation).bind(thread).bind(&turn).fetch_one(&mut *tx).await?;
            let active = active && parent.is_none();
            let destination = parent.as_deref().unwrap_or(&runtime_conversation);
            let created = now.to_string();
            let message = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) VALUES (?,?,?,?,?,?,'accepted_by_wonder',?)")
                .bind(&message).bind(device).bind(format!("question-{id}")).bind(body).bind(hex::encode(Sha256::digest(body.as_bytes()))).bind(destination).bind(&created).execute(&mut *tx).await?;
            if active {
                sqlx::query("INSERT INTO guide_work(message_id,expected_turn_id) VALUES (?,?)")
                    .bind(&message)
                    .bind(turn)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("INSERT INTO dispatch_work(message_id) VALUES (?)")
                    .bind(&message)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE async_questions SET message_id=? WHERE id=?")
                .bind(message)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> Store {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("phone", "Phone", "{}", "0")
            .await
            .unwrap();
        store
    }
    async fn question(store: &Store, item: &str) -> String {
        store
            .save_async_question(
                "chat",
                "thread",
                "turn",
                item,
                "[{\"title\":\"Which day?\"}]",
                1000,
            )
            .await
            .unwrap();
        store
            .async_questions("chat", 0)
            .await
            .unwrap()
            .into_iter()
            .find(|q| q.item_id == item)
            .unwrap()
            .id
    }
    #[tokio::test]
    async fn reply_is_atomic_idempotent_and_preserves_guide_on_restart() {
        let store = setup().await;
        let MessageInsert::Inserted(active) = store
            .insert_message("phone", "active", "work", "hash", "chat", "0")
            .await
            .unwrap()
        else {
            panic!()
        };
        store
            .update_message_delivery(&active.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let id = question(&store, "a").await;
        let (first, other) = tokio::join!(
            store.answer_async_question("chat", &id, "phone", "Friday", Some("Friday"), 1),
            store.answer_async_question("chat", &id, "phone", "Saturday", Some("Saturday"), 1)
        );
        assert_ne!(first.unwrap(), other.unwrap());
        store.recover_dispatch_claims().await.unwrap();
        let guides = store.pending_guides().await.unwrap();
        assert_eq!(guides.len(), 1);
        let hidden = store.question_answer_messages("chat").await.unwrap();
        assert_eq!(hidden.len(), 1);
        assert_ne!(hidden[0].id, active.id);
        assert_eq!(guides[0].1, "turn");
        let body = &guides[0].0.body;
        assert!(store
            .answer_async_question("chat", &id, "phone", body, Some(body), 2000)
            .await
            .unwrap());
        assert!(!store
            .answer_async_question("chat", &id, "phone", "skip", None, 2)
            .await
            .unwrap());
        assert_eq!(store.pending_guides().await.unwrap().len(), 1);
        assert!(store.pending_dispatch_messages().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn skip_expiry_and_replay_never_generate_work() {
        let store = setup().await;
        let id = question(&store, "a").await;
        assert!(store
            .answer_async_question("chat", &id, "phone", "skip", None, 1)
            .await
            .unwrap());
        store
            .save_async_question("chat", "thread", "turn", "a", "[]", 9000)
            .await
            .unwrap();
        assert_eq!(
            store.async_questions("chat", 0).await.unwrap()[0].state,
            "dismissed"
        );
        let id = question(&store, "b").await;
        assert!(!store
            .answer_async_question("chat", &id, "phone", "late", Some("late"), 1000)
            .await
            .unwrap());
        assert_eq!(
            store.async_questions("chat", 1000).await.unwrap()[0].state,
            "expired"
        );
        assert!(store.pending_dispatch_messages().await.unwrap().is_empty());
        assert!(store.pending_guides().await.unwrap().is_empty());
        assert!(!store
            .answer_async_question("other", &id, "phone", "reply", Some("reply"), 1)
            .await
            .unwrap());
    }
    #[tokio::test]
    async fn pending_list_drops_answered_and_expired_questions() {
        let store = setup().await;
        let open = question(&store, "open").await;
        let answered = question(&store, "answered").await;
        assert!(store
            .answer_async_question("chat", &answered, "phone", "reply", Some("reply"), 1)
            .await
            .unwrap());
        let pending = store.pending_async_questions(1).await.unwrap();
        assert_eq!(
            pending.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
            [open.as_str()]
        );
        assert!(store
            .pending_async_questions(1000)
            .await
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn child_answer_preserves_question_ownership_and_only_queues_parent_work() {
        let store = setup().await;
        seed_project_thread(
            &store,
            "project",
            "Project",
            "/tmp/child-question-test",
            None,
            None,
            "0",
        )
        .await
        .unwrap();
        seed_project_thread(
            &store,
            "parent",
            "Parent",
            "/tmp/child-question-parent",
            None,
            None,
            "0",
        )
        .await
        .unwrap();
        store
            .bind_project_runtime(
                "parent",
                AgentFamily::Codex,
                "fixture-store",
                "parent-thread",
                None,
                "0",
            )
            .await
            .unwrap();
        store
            .register_subagent_ownership(
                "chat",
                "parent",
                "thread",
                "parent-thread",
                None,
                None,
                None,
                "{}",
                Some("runtime"),
                Some(false),
                "completed",
                Some(false),
                "0",
            )
            .await
            .unwrap();
        let id = question(&store, "child-question").await;
        assert!(store
            .answer_async_question("chat", &id, "phone", "Friday", Some("Friday"), 1)
            .await
            .unwrap());
        let queued = store.pending_dispatch_messages().await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].conversation_id, "parent");
        assert!(store.pending_guides().await.unwrap().is_empty());
        let owner: String =
            sqlx::query_scalar("SELECT conversation_id FROM async_questions WHERE id=?")
                .bind(id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(owner, "chat");
        assert!(store
            .messages_for_conversation("chat")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn completed_turn_answer_uses_normal_durable_send() {
        let store = setup().await;
        let id = question(&store, "a").await;
        assert!(store
            .answer_async_question("chat", &id, "phone", "Friday", Some("Friday"), 1)
            .await
            .unwrap());
        assert_eq!(store.pending_dispatch_messages().await.unwrap().len(), 1);
        assert!(store.pending_guides().await.unwrap().is_empty());
    }
}
