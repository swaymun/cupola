//! Rewinding a Project conversation to before one of its turns ("Edit from
//! here"), and the owner's per-provider switches it depends on.
//!
//! A durable intent fences sends and history before the provider changes.
//! The provider then rewinds its history (Codex `thread/revert`, Claude through
//! Wonder's bridge); settlement records the cut and releases the fence together. Like a native reset, the removed turns are hidden rather than
//! deleted: receipts stay for audit and idempotent retries, the history
//! revision fences paging and client caches, and the hydration token is
//! cleared so a refresh already in flight cannot save the removed turns again.

use crate::{AgentFamily, Store};
use sqlx::Row;

/// Durable before-image of a provider rewind. Turns are newest first and
/// complete: recovery may only settle the exact cut or roll back an unchanged
/// provider history. A partial/ambiguous read never releases the fence.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RewindOperation {
    pub id: String,
    pub conversation: String,
    pub thread: String,
    pub native_session: String,
    pub runtime_id: String,
    pub turn: String,
    pub turns: Vec<String>,
    /// A returned provider call is safe to inspect in its current process.
    /// Without this receipt, recovery waits for a replacement runtime: an
    /// abandoned RPC might still be mutating its in-memory history.
    #[serde(skip)]
    pub response_received: bool,
    /// When the daemon's call ended without a usable reply (millisecond
    /// timestamp). Nothing of this process is still waiting on the provider.
    #[serde(skip)]
    pub call_ended_at: Option<String>,
    #[serde(skip)]
    pub created_at: String,
}

/// What a store fence says while a rewind is unresolved. Callers answer it
/// as a temporary, plain-language state, never as a server fault.
pub const REWIND_PENDING: &str = "Rewind recovery is pending. Try again shortly.";

pub fn is_rewind_pending(error: &sqlx::Error) -> bool {
    error.to_string().contains(REWIND_PENDING)
}

impl RewindOperation {
    pub fn removed_turns(&self) -> Option<&[String]> {
        self.turns
            .iter()
            .position(|turn| turn == &self.turn)
            .map(|index| &self.turns[..=index])
    }

    pub fn retained_turns(&self) -> Option<&[String]> {
        self.turns
            .iter()
            .position(|turn| turn == &self.turn)
            .map(|index| &self.turns[index + 1..])
    }
}

/// A provider setting the owner turns on per Mac. A missing row is off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSetting {
    /// The provider keeps backups of files it edits in Wonder's chats, so a
    /// rewind can restore them (Claude's `enableFileCheckpointing`).
    FileCheckpointing,
    /// Claude only: Cupola adds the chats it starts to the Claude app and
    /// closes the app's copy of a chat before sending to it. Off: a chat open
    /// in the Claude app waits, like one open in a terminal.
    ClaudeAppSync,
}

impl ProviderSetting {
    fn key(self) -> &'static str {
        match self {
            Self::FileCheckpointing => "file_checkpointing",
            Self::ClaudeAppSync => "claude_app_sync",
        }
    }
}

/// One rewind of a Project conversation, as the provider reported it.
#[derive(Clone, Copy, Debug)]
pub struct ProjectRewind<'a> {
    pub conversation: &'a str,
    /// The runtime thread the turns belong to (the conversation's binding).
    pub thread: &'a str,
    /// Every turn the provider removed: the chosen one and all after it.
    pub removed_turns: &'a [String],
    /// `(old, new)` when the provider moved the conversation to a new native
    /// session (a Claude rewind of the first message starts a fresh one).
    pub session: Option<(&'a str, &'a str)>,
    pub now: &'a str,
}

impl Store {
    pub async fn pending_rewinds(&self) -> Result<Vec<RewindOperation>, sqlx::Error> {
        let rows = sqlx::query("SELECT intent_json,response_received,call_ended_at,created_at FROM rewind_operations ORDER BY created_at,conversation_id")
            .fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let mut operation: RewindOperation =
                    serde_json::from_str(&row.get::<String, _>("intent_json"))
                        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                operation.response_received = row.get("response_received");
                operation.call_ended_at = row.get("call_ended_at");
                operation.created_at = row.get("created_at");
                Ok(operation)
            })
            .collect()
    }

    pub async fn record_rewind_response(
        &self,
        operation: &RewindOperation,
    ) -> Result<(), sqlx::Error> {
        let result = sqlx::query("UPDATE rewind_operations SET response_received=1 WHERE operation_id=? AND conversation_id=?")
            .bind(&operation.id).bind(&operation.conversation).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol("Rewind operation changed".into()));
        }
        Ok(())
    }

    /// The daemon's provider call ended without a usable reply. Best effort:
    /// without it recovery waits from the operation's creation instead.
    pub async fn record_rewind_call_end(
        &self,
        operation: &RewindOperation,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rewind_operations SET call_ended_at=? WHERE operation_id=? AND conversation_id=?")
            .bind(now).bind(&operation.id).bind(&operation.conversation).execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn require_settled_rewind(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        conversation: &str,
    ) -> Result<(), sqlx::Error> {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=?)",
        )
        .bind(conversation)
        .fetch_one(&mut **tx)
        .await?;
        if pending {
            return Err(sqlx::Error::Protocol(REWIND_PENDING.into()));
        }
        Ok(())
    }

    pub async fn begin_project_rewind(
        &self,
        operation: &RewindOperation,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        if operation.removed_turns().is_none()
            || operation.turns.len() > 5_000
            || operation
                .turns
                .iter()
                .any(|turn| turn.is_empty() || turn.len() > 128)
            || operation
                .turns
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != operation.turns.len()
        {
            return Err(sqlx::Error::Protocol("Invalid rewind before-image".into()));
        }
        let intent = serde_json::to_string(operation)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let bound: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_conversations p JOIN runtime_bindings b USING(conversation_id) WHERE p.conversation_id=? AND b.runtime_thread_id=? AND p.native_session_id=? AND b.execution_scope='projects' AND b.agent_family=p.agent_family AND b.provider_store=p.provider_store AND (b.session_id IS NULL OR b.session_id=p.native_session_id))")
            .bind(&operation.conversation).bind(&operation.thread).bind(&operation.native_session).fetch_one(&mut *tx).await?;
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming','uncertain'))")
            .bind(&operation.conversation).fetch_one(&mut *tx).await?;
        if !bound || busy {
            return Err(sqlx::Error::Protocol(
                "Rewind binding changed or work is pending".into(),
            ));
        }
        sqlx::query("INSERT INTO rewind_operations(conversation_id,operation_id,intent_json,created_at) VALUES(?,?,?,?)")
            .bind(&operation.conversation).bind(&operation.id).bind(intent).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE history_hydration SET state='idle',token=NULL,file_change_version=0 WHERE conversation_id=?")
            .bind(&operation.conversation).execute(&mut *tx).await?;
        // Invalidate cached projections as soon as the provider can change.
        sqlx::query("UPDATE conversations SET history_revision=history_revision+1 WHERE id=?")
            .bind(&operation.conversation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    /// Roll back only after the runtime proves it did not change history.
    pub async fn cancel_project_rewind(
        &self,
        operation: &RewindOperation,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::remove_rewind_operation(&mut tx, operation).await?;
        sqlx::query("UPDATE conversations SET history_revision=history_revision+1 WHERE id=?")
            .bind(&operation.conversation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    async fn remove_rewind_operation(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        operation: &RewindOperation,
    ) -> Result<(), sqlx::Error> {
        let intent = serde_json::to_string(operation)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let removed = sqlx::query("DELETE FROM rewind_operations WHERE conversation_id=? AND operation_id=? AND intent_json=?")
            .bind(&operation.conversation).bind(&operation.id).bind(intent).execute(&mut **tx).await?;
        if removed.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol("Rewind operation changed".into()));
        }
        Ok(())
    }

    pub async fn settle_project_rewind(
        &self,
        operation: &RewindOperation,
        session: Option<&str>,
        now: &str,
    ) -> Result<i64, sqlx::Error> {
        let removed = operation
            .removed_turns()
            .ok_or_else(|| sqlx::Error::Protocol("Invalid rewind before-image".into()))?;
        self.settle_project_rewind_cut(operation, removed, session, now)
            .await
    }

    /// Settles against what the provider now holds: `hidden` are the
    /// before-image turns it no longer has. Equal to the planned cut unless
    /// the owner changed the native thread meanwhile.
    pub async fn settle_project_rewind_cut(
        &self,
        operation: &RewindOperation,
        hidden: &[String],
        session: Option<&str>,
        now: &str,
    ) -> Result<i64, sqlx::Error> {
        if hidden.iter().any(|turn| !operation.turns.contains(turn)) {
            return Err(sqlx::Error::Protocol("Invalid rewind cut".into()));
        }
        self.apply_project_rewind(
            ProjectRewind {
                conversation: &operation.conversation,
                thread: &operation.thread,
                removed_turns: hidden,
                session: session
                    .filter(|new| *new != operation.native_session)
                    .map(|new| (operation.native_session.as_str(), new)),
                now,
            },
            Some(operation),
        )
        .await
    }

    /// Computer-wide switch; no saved row means on. Native ChatGPT computer
    /// use does not read this preference.
    pub async fn cua_driver_enabled(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE((SELECT cua_driver_enabled FROM computer_use_settings WHERE id=1),1)",
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn set_cua_driver_enabled(
        &self,
        enabled: bool,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO computer_use_settings(id,cua_driver_enabled,updated_at) VALUES(1,?,?) ON CONFLICT(id) DO UPDATE SET cua_driver_enabled=excluded.cua_driver_enabled,updated_at=excluded.updated_at")
            .bind(enabled).bind(now).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn provider_setting(
        &self,
        family: AgentFamily,
        setting: ProviderSetting,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM provider_settings WHERE agent_family=? AND setting=?)",
        )
        .bind(family.as_str())
        .bind(setting.key())
        .fetch_one(&self.pool)
        .await
    }

    /// Turning a setting off removes its row, returning it to the default.
    pub async fn set_provider_setting(
        &self,
        family: AgentFamily,
        setting: ProviderSetting,
        enabled: bool,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        if enabled {
            sqlx::query("INSERT INTO provider_settings(agent_family,setting,enabled,updated_at) VALUES(?,?,1,?) ON CONFLICT(agent_family,setting) DO UPDATE SET updated_at=excluded.updated_at")
                .bind(family.as_str()).bind(setting.key()).bind(now)
                .execute(&self.pool).await?;
        } else {
            sqlx::query("DELETE FROM provider_settings WHERE agent_family=? AND setting=?")
                .bind(family.as_str())
                .bind(setting.key())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Records a provider's rewind in one transaction and returns the new
    /// history revision. Refused while the conversation has delivered or
    /// uncertain work, or when its binding is no longer `thread`
    /// (and, for a session change, no longer the old session).
    pub async fn rewind_project_history(
        &self,
        rewind: ProjectRewind<'_>,
    ) -> Result<i64, sqlx::Error> {
        self.apply_project_rewind(rewind, None).await
    }

    async fn apply_project_rewind(
        &self,
        rewind: ProjectRewind<'_>,
        operation: Option<&RewindOperation>,
    ) -> Result<i64, sqlx::Error> {
        let ProjectRewind {
            conversation,
            thread,
            removed_turns,
            session,
            now,
        } = rewind;
        if (removed_turns.is_empty() && operation.is_none())
            || removed_turns.len() > 10_000
            || removed_turns
                .iter()
                .any(|turn| turn.is_empty() || turn.len() > 128)
        {
            return Err(sqlx::Error::Protocol("Invalid rewound turns".into()));
        }
        if let Some((old, new)) = session {
            if old == new
                || uuid::Uuid::parse_str(old).is_err()
                || uuid::Uuid::parse_str(new).is_err()
            {
                return Err(sqlx::Error::Protocol("Invalid rewind session".into()));
            }
        }
        let removed =
            serde_json::to_string(removed_turns).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(operation) = operation {
            // Deletion and the cut commit together; every error restores the
            // intent and all fences, including a failed binding/session write.
            Self::remove_rewind_operation(&mut tx, operation).await?;
        } else {
            Self::require_settled_rewind(&mut tx, conversation).await?;
        }
        let row = sqlx::query("SELECT p.native_session_id,b.session_id,b.runtime_thread_id FROM project_conversations p JOIN runtime_bindings b USING(conversation_id) WHERE p.conversation_id=? AND b.execution_scope='projects' AND b.provider_store=p.provider_store AND b.agent_family=p.agent_family")
            .bind(conversation).fetch_optional(&mut *tx).await?
            .ok_or_else(|| sqlx::Error::Protocol("Rewind has no Project binding".into()))?;
        if row.get::<String, _>("runtime_thread_id") != thread {
            return Err(sqlx::Error::Protocol("Rewind binding changed".into()));
        }
        // A message queued meanwhile has not entered the provider's history;
        // it is sent after the rewind.
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ('dispatching_to_codex','accepted_by_codex','streaming','uncertain'))")
            .bind(conversation).fetch_one(&mut *tx).await?;
        if busy {
            return Err(sqlx::Error::Protocol(
                "Rewind refused while work runs".into(),
            ));
        }
        if let Some((old, new)) = session {
            let native: Option<String> = row.get("native_session_id");
            let bound: Option<String> = row.get("session_id");
            if native.as_deref() != Some(old) || bound.as_deref() != Some(old) {
                return Err(sqlx::Error::Protocol("Rewind session changed".into()));
            }
            sqlx::query("INSERT INTO project_runtime_switch_guard(conversation_id) VALUES(?)")
                .bind(conversation)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE project_conversations SET native_session_id=?,native_settings_turn=NULL,updated_at=? WHERE conversation_id=?")
                .bind(new).bind(now).bind(conversation).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE runtime_bindings SET session_id=?,updated_at=? WHERE conversation_id=?",
            )
            .bind(new)
            .bind(now)
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE conversations SET session_id=? WHERE id=?")
                .bind(new)
                .bind(conversation)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM project_runtime_switch_guard WHERE conversation_id=?")
                .bind(conversation)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT OR IGNORE INTO rewound_turns(conversation_id,thread_id,turn_id) SELECT ?,?,value FROM json_each(?)")
            .bind(conversation).bind(thread).bind(&removed).execute(&mut *tx).await?;
        // The removed turns, by identity: the owner's messages, replies and
        // every saved item. The reset triggers keep late items of a hidden
        // turn hidden.
        sqlx::query("UPDATE history_entries SET hidden_by_reset=1 WHERE conversation_id=?1 AND hidden_by_reset=0 AND (
            (source='message' AND source_id IN (SELECT id FROM messages WHERE conversation_id=?1 AND codex_thread_id=?2 AND codex_turn_id IN (SELECT value FROM json_each(?3))))
            OR (source='assistant' AND source_id IN (SELECT id FROM assistant_messages WHERE conversation_id=?1 AND codex_thread_id=?2 AND codex_turn_id IN (SELECT value FROM json_each(?3))))
            OR (source='event' AND json_extract(payload_json,'$.threadId')=?2 AND json_extract(payload_json,'$.turnId') IN (SELECT value FROM json_each(?3))))")
            .bind(conversation).bind(thread).bind(&removed).execute(&mut *tx).await?;
        sqlx::query("UPDATE project_conversations SET native_settings_turn=NULL WHERE conversation_id=? AND native_settings_turn IN (SELECT value FROM json_each(?))")
            .bind(conversation).bind(&removed).execute(&mut *tx).await?;
        // Questions and approvals of a removed turn can no longer be answered.
        sqlx::query("UPDATE approvals SET state='resolved',decision='conversation_reset',resolved_at=?,resolution_json=NULL WHERE thread_id=? AND state IN ('pending','resolving') AND turn_id IN (SELECT value FROM json_each(?))")
            .bind(now).bind(thread).bind(&removed).execute(&mut *tx).await?;
        sqlx::query("UPDATE async_questions SET state='expired' WHERE thread_id=? AND state='pending' AND turn_id IN (SELECT value FROM json_each(?))")
            .bind(thread).bind(&removed).execute(&mut *tx).await?;
        sqlx::query("UPDATE conversations SET history_revision=history_revision+1 WHERE id=?")
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        // A refresh already reading the old native history is superseded.
        sqlx::query("UPDATE history_hydration SET state='idle',token=NULL,file_change_version=0 WHERE conversation_id=?")
            .bind(conversation).execute(&mut *tx).await?;
        let revision: i64 =
            sqlx::query_scalar("SELECT history_revision FROM conversations WHERE id=?")
                .bind(conversation)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageInsert;
    use wonder_api::{HostEventEnvelope, WonderEvent};

    // Upgrade a populated pre-journal database, then prove that a failed
    // settlement rolls back the cut and the fence deletion together across
    // a real close/reopen. Stale operation IDs cannot release another hold.
    #[tokio::test]
    async fn rewind_journal_migration_and_failed_settlement_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("journal.db").display()
        );
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 151)
                .cloned()
                .collect::<Vec<_>>(),
        )
        .run(&pool)
        .await
        .unwrap();
        let store = Store { pool };
        crate::provider_switch::tests::thread(&store, AgentFamily::Codex, Some("thread")).await;
        store.start_event_epoch("epoch").await.unwrap();
        store
            .bind_project_runtime("c1", AgentFamily::Codex, "codex:home", "thread", None, "1")
            .await
            .unwrap();
        store
            .upsert_owner_device("owner", "Phone", "{}", "1")
            .await
            .unwrap();
        let kept = sent(&store, "first", "turn-1", "2").await;
        let removed = sent(&store, "second", "turn-2", "3").await;
        migrations.run(&store.pool).await.unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            page.messages.len(),
            2,
            "migration preserves populated history"
        );
        assert!(store.pending_rewinds().await.unwrap().is_empty());
        assert!(store
            .claim_history_refresh("c1", "before", 100)
            .await
            .unwrap());
        let operation = RewindOperation {
            id: "operation".into(),
            conversation: "c1".into(),
            thread: "thread".into(),
            native_session: "thread".into(),
            runtime_id: "runtime".into(),
            turn: "turn-2".into(),
            turns: vec!["turn-3".into(), "turn-2".into(), "turn-1".into()],
            ..Default::default()
        };
        store.begin_project_rewind(&operation, "4").await.unwrap();
        assert!(store.begin_project_rewind(&operation, "4").await.is_err());
        assert!(!store
            .history_refresh_is_current("c1", "before")
            .await
            .unwrap());
        assert!(!store
            .claim_history_refresh("c1", "during", 200)
            .await
            .unwrap());
        assert!(store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .is_err());
        assert!(store
            .insert_dispatch_message("owner", "new", "next", "next", "c1", &[], "4", true)
            .await
            .is_err());
        // The database fence itself, below the transition guard (a final
        // message cannot return to work through `update_message_delivery`).
        assert!(
            sqlx::query("UPDATE messages SET state='streaming' WHERE id=?")
                .bind(&removed)
                .execute(&store.pool)
                .await
                .is_err()
        );
        let mut wrong = operation.clone();
        wrong.id = "different".into();
        assert!(store.cancel_project_rewind(&wrong).await.is_err());
        store.record_rewind_response(&operation).await.unwrap();
        sqlx::query("CREATE TRIGGER fail_cut BEFORE UPDATE OF hidden_by_reset ON history_entries BEGIN SELECT RAISE(ABORT,'fault'); END").execute(&store.pool).await.unwrap();
        assert!(store
            .settle_project_rewind(&operation, None, "5")
            .await
            .is_err());
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        assert_eq!(store.pending_rewinds().await.unwrap()[0].id, operation.id);
        assert!(store.pending_rewinds().await.unwrap()[0].response_received);
        assert!(store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .is_err());
        sqlx::query("DROP TRIGGER fail_cut")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .settle_project_rewind(&operation, None, "6")
                .await
                .unwrap(),
            2
        );
        assert!(store
            .settle_project_rewind(&operation, None, "6")
            .await
            .is_err());
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            [kept.as_str()]
        );
        assert_eq!(page.history_revision, 2, "settlement is exactly once");
        assert!(store.pending_rewinds().await.unwrap().is_empty());
        store
            .commit_event(&mut item(30, "turn-3", "never-indexed-before-the-cut"))
            .await
            .unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            page.events.is_empty(),
            "late history cannot introduce a removed turn for the first time"
        );
        assert!(store
            .runtime_turn_hidden_by_reset("thread", "turn-3")
            .await
            .unwrap());
        assert!(store
            .claim_history_refresh("c1", "after", 300)
            .await
            .unwrap());
    }

    async fn fenced_store() -> (Store, RewindOperation, [String; 2]) {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        crate::provider_switch::tests::thread(&store, AgentFamily::Codex, Some("thread")).await;
        store.start_event_epoch("epoch").await.unwrap();
        store
            .bind_project_runtime("c1", AgentFamily::Codex, "codex:home", "thread", None, "1")
            .await
            .unwrap();
        store
            .upsert_owner_device("owner", "Phone", "{}", "1")
            .await
            .unwrap();
        let first = sent(&store, "first", "turn-1", "2").await;
        let second = sent(&store, "second", "turn-2", "3").await;
        let operation = RewindOperation {
            id: "operation".into(),
            conversation: "c1".into(),
            thread: "thread".into(),
            native_session: "thread".into(),
            runtime_id: "runtime".into(),
            turn: "turn-2".into(),
            turns: vec!["turn-2".into(), "turn-1".into()],
            ..Default::default()
        };
        store.begin_project_rewind(&operation, "4").await.unwrap();
        (store, operation, [first, second])
    }

    // Contract: the fence reports itself as a typed, recoverable state; the
    // call-end receipt survives reopen; unbinding a fenced chat is refused but
    // deleting the whole chat is not (the cascade removes the binding first).
    #[tokio::test]
    async fn fenced_rewind_reports_pending_and_the_chat_can_still_be_deleted() {
        let (store, operation, _) = fenced_store().await;
        let error = store
            .insert_dispatch_message("owner", "new", "next", "next", "c1", &[], "5", true)
            .await
            .unwrap_err();
        assert!(is_rewind_pending(&error), "{error}");
        let Err(error) = store.committed_conversation_snapshot("epoch", "c1").await else {
            panic!("a fenced chat must not return a snapshot");
        };
        assert!(is_rewind_pending(&error), "{error}");
        let pending = store.pending_rewinds().await.unwrap();
        assert_eq!(pending[0].created_at, "4");
        assert_eq!(pending[0].call_ended_at, None);
        store.record_rewind_call_end(&operation, "9").await.unwrap();
        assert_eq!(
            store.pending_rewinds().await.unwrap()[0]
                .call_ended_at
                .as_deref(),
            Some("9")
        );
        assert!(
            sqlx::query("DELETE FROM runtime_bindings WHERE conversation_id='c1'")
                .execute(&store.pool)
                .await
                .is_err(),
            "unbinding a chat that stays would escape the fence"
        );
        sqlx::query("DELETE FROM conversations WHERE id='c1'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.pending_rewinds().await.unwrap().is_empty());
    }

    // Contract: settlement may hide only before-image turns, and a provider
    // history that is neither the cut nor the original still settles to what
    // the provider holds, releasing the fence once.
    #[tokio::test]
    async fn rewind_settles_to_the_providers_actual_history() {
        let (store, operation, [first, second]) = fenced_store().await;
        assert!(store
            .settle_project_rewind_cut(&operation, &["unrelated".into()], None, "5")
            .await
            .is_err());
        assert_eq!(store.pending_rewinds().await.unwrap().len(), 1);
        // The owner replaced the whole thread natively: nothing remains.
        store
            .settle_project_rewind_cut(&operation, &["turn-2".into(), "turn-1".into()], None, "5")
            .await
            .unwrap();
        assert!(store.pending_rewinds().await.unwrap().is_empty());
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert!(page.messages.is_empty(), "{first} {second}");
        assert!(store
            .settle_project_rewind_cut(&operation, &[], None, "6")
            .await
            .is_err());
    }

    // Contract: old backend choices were never an off switch. Upgrade defaults
    // Cua Driver on, preserves unrelated settings and persists later changes.
    #[tokio::test]
    async fn cua_driver_toggle_migrates_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("settings.db").display()
        );
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let migrations = sqlx::migrate!();
        let previous = sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 117)
                .cloned()
                .collect(),
        );
        previous.run(&pool).await.unwrap();
        sqlx::raw_sql("INSERT INTO provider_settings VALUES ('codex','desktop_computer_use',1,'old'),('claude','file_checkpointing',1,'old')")
            .execute(&pool).await.unwrap();
        migrations.run(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM provider_settings WHERE setting='desktop_computer_use'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        let store = Store { pool };
        assert!(store.cua_driver_enabled().await.unwrap());
        assert!(store
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
        store.set_cua_driver_enabled(false, "now").await.unwrap();
        store.pool.close().await;
        let reopened = Store::connect(&url).await.unwrap();
        assert!(!reopened.cua_driver_enabled().await.unwrap());
        reopened
            .set_cua_driver_enabled(true, "later")
            .await
            .unwrap();
        assert!(reopened.cua_driver_enabled().await.unwrap());
    }

    // Contract: Claude app sync is off until the owner turns it on, only for
    // Claude; the upgrade keeps the existing rows and the setting persists.
    #[tokio::test]
    async fn claude_app_sync_defaults_off_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("settings.db").display()
        );
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 125)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO provider_settings VALUES ('claude','file_checkpointing',1,'old')",
        )
        .execute(&pool)
        .await
        .unwrap();
        migrations.run(&pool).await.unwrap();
        assert!(
            sqlx::raw_sql(
                "INSERT INTO provider_settings VALUES ('codex','claude_app_sync',1,'now')"
            )
            .execute(&pool)
            .await
            .is_err(),
            "only Claude has the Claude app"
        );
        let store = Store { pool };
        let sync = || store.provider_setting(AgentFamily::Claude, ProviderSetting::ClaudeAppSync);
        assert!(!sync().await.unwrap(), "off by default");
        assert!(store
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
        store
            .set_provider_setting(
                AgentFamily::Claude,
                ProviderSetting::ClaudeAppSync,
                true,
                "now",
            )
            .await
            .unwrap();
        assert!(sync().await.unwrap());
        store.pool.close().await;
        let reopened = Store::connect(&url).await.unwrap();
        assert!(reopened
            .provider_setting(AgentFamily::Claude, ProviderSetting::ClaudeAppSync)
            .await
            .unwrap());
        reopened
            .set_provider_setting(
                AgentFamily::Claude,
                ProviderSetting::ClaudeAppSync,
                false,
                "later",
            )
            .await
            .unwrap();
        assert!(!reopened
            .provider_setting(AgentFamily::Claude, ProviderSetting::ClaudeAppSync)
            .await
            .unwrap());
        assert!(reopened
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
    }

    fn item(sequence: u64, turn: &str, item: &str) -> HostEventEnvelope {
        HostEventEnvelope {
            event_id: format!("event-{sequence}"),
            host_epoch: "epoch".into(),
            sequence,
            occurred_at: format!("{}", 1_700_000_000_000_u64 + sequence),
            conversation_id: Some("c1".into()),
            thread_id: Some("thread".into()),
            turn_id: Some(turn.into()),
            item_id: Some(item.into()),
            request_id: None,
            device_id: None,
            message_id: None,
            approval_id: None,
            event: WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(
                    serde_json::json!({"itemId":item,"turnId":turn,"state":"completed",
                        "item":{"id":item,"type":"agentMessage","text":item}})
                    .to_string(),
                ),
            },
        }
    }

    async fn sent(store: &Store, client: &str, turn: &str, at: &str) -> String {
        sent_on(store, "thread", client, turn, at).await
    }

    async fn sent_on(store: &Store, thread: &str, client: &str, turn: &str, at: &str) -> String {
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message("owner", client, client, client, "c1", &[], at, true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        store
            .update_message_delivery(&message.id, "completed", Some(thread), Some(turn))
            .await
            .unwrap();
        message.id
    }

    // Contract: a rewind hides exactly the removed turns (messages, replies
    // and items), keeps them hidden when a late item arrives, fences old
    // cursors and in-flight refreshes, and is refused while work runs or after
    // the binding moved. Settings default off and persist.
    #[tokio::test]
    async fn rewind_hides_removed_turns_and_fences_history() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        crate::provider_switch::tests::thread(&store, AgentFamily::Codex, Some("thread")).await;
        store.start_event_epoch("epoch").await.unwrap();
        store
            .bind_project_runtime("c1", AgentFamily::Codex, "codex:home", "thread", None, "1")
            .await
            .unwrap();
        store
            .upsert_owner_device("owner", "Phone", "{}", "1")
            .await
            .unwrap();
        let kept = sent(&store, "first", "turn-1", "2").await;
        store
            .commit_event(&mut item(1, "turn-1", "kept-reply"))
            .await
            .unwrap();
        let removed = sent(&store, "second", "turn-2", "3").await;
        store
            .commit_event(&mut item(2, "turn-2", "removed-reply"))
            .await
            .unwrap();
        let before = store
            .conversation_history_page("epoch", "c1", None, 1)
            .await
            .unwrap()
            .unwrap();
        let old_cursor = before.next_cursor.unwrap();
        assert!(store
            .claim_history_refresh("c1", "reader", 100)
            .await
            .unwrap());

        let rewind = |thread: &'static str, turns: &'static [&'static str]| {
            let store = store.clone();
            async move {
                let turns: Vec<String> = turns.iter().map(|t| (*t).to_owned()).collect();
                store
                    .rewind_project_history(ProjectRewind {
                        conversation: "c1",
                        thread,
                        removed_turns: &turns,
                        session: None,
                        now: "4",
                    })
                    .await
            }
        };
        assert!(rewind("other-thread", &["turn-2"]).await.is_err());
        let MessageInsert::Inserted(running) = store
            .insert_dispatch_message("owner", "running", "next", "next", "c1", &[], "3", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        store
            .update_message_delivery(&running.id, "streaming", Some("thread"), Some("turn-3"))
            .await
            .unwrap();
        assert!(
            rewind("thread", &["turn-2"]).await.is_err(),
            "running work refuses"
        );
        store
            .update_message_delivery(&running.id, "interrupted", Some("thread"), Some("turn-3"))
            .await
            .unwrap();
        assert_eq!(rewind("thread", &["turn-2"]).await.unwrap(), 1);

        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.history_revision, 1);
        let ids: Vec<_> = page.messages.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&kept.as_str()) && !ids.contains(&removed.as_str()));
        let items: Vec<_> = page
            .events
            .iter()
            .filter_map(|e| e.item_id.as_deref())
            .collect();
        assert_eq!(items, ["kept-reply"]);
        assert!(store
            .conversation_history_page("epoch", "c1", Some(&old_cursor), 1)
            .await
            .is_err());
        assert!(!store
            .history_refresh_is_current("c1", "reader")
            .await
            .unwrap());

        // A late item of the removed turn stays hidden.
        store
            .commit_event(&mut item(3, "turn-2", "late-reply"))
            .await
            .unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert!(page
            .events
            .iter()
            .all(|e| e.turn_id.as_deref() == Some("turn-1")));

        assert!(!store
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
        store
            .set_provider_setting(
                AgentFamily::Claude,
                ProviderSetting::FileCheckpointing,
                true,
                "5",
            )
            .await
            .unwrap();
        assert!(store
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
        assert!(!store
            .provider_setting(AgentFamily::Codex, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
        store
            .set_provider_setting(
                AgentFamily::Claude,
                ProviderSetting::FileCheckpointing,
                false,
                "6",
            )
            .await
            .unwrap();
        assert!(!store
            .provider_setting(AgentFamily::Claude, ProviderSetting::FileCheckpointing)
            .await
            .unwrap());
    }

    // Contract: a rewind that starts a fresh native session moves the binding
    // only from the session it was made against.
    #[tokio::test]
    async fn rewind_to_a_fresh_session_moves_the_exact_binding() {
        let old = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let new = "11111111-2222-4333-8444-555555555555";
        let store = Store::connect("sqlite::memory:").await.unwrap();
        crate::provider_switch::tests::thread(&store, AgentFamily::Claude, Some(old)).await;
        store.start_event_epoch("epoch").await.unwrap();
        store
            .bind_project_runtime(
                "c1",
                AgentFamily::Claude,
                "claude:home",
                "claude-bridge",
                Some(old),
                "1",
            )
            .await
            .unwrap();
        store
            .upsert_owner_device("owner", "Phone", "{}", "1")
            .await
            .unwrap();
        sent_on(&store, "claude-bridge", "first", "turn-1", "2").await;
        let turns = vec!["turn-1".to_owned()];
        let rewind = |session: (&'static str, &'static str)| {
            let store = store.clone();
            let turns = turns.clone();
            async move {
                store
                    .rewind_project_history(ProjectRewind {
                        conversation: "c1",
                        thread: "claude-bridge",
                        removed_turns: &turns,
                        session: Some(session),
                        now: "3",
                    })
                    .await
            }
        };
        assert!(rewind((new, old)).await.is_err(), "not the bound session");
        assert_eq!(rewind((old, new)).await.unwrap(), 1);
        let stored = store.project_conversation("c1").await.unwrap().unwrap();
        assert_eq!(stored.native_session_id.as_deref(), Some(new));
        let binding = store.runtime_binding("c1").await.unwrap().unwrap();
        assert_eq!(binding.session_id.as_deref(), Some(new));
        let page = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert!(page.messages.is_empty());
    }
}
