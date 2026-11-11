//! Permanent deletion of Cupola's copy; provider stores and project files stay intact.
use super::*;
use std::collections::HashSet;

#[derive(Debug, PartialEq, Eq)]
pub enum DeleteProjectChat {
    Deleted,
    Missing,
    Working,
}

impl Store {
    pub async fn deleted_project_sessions(
        &self,
        family: AgentFamily,
        store: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT native_session_id FROM deleted_project_sessions WHERE agent_family=? AND provider_store=?")
            .bind(family.as_str()).bind(store).fetch_all(&self.pool).await
    }

    pub async fn project_session_deleted(
        &self,
        family: AgentFamily,
        store: &str,
        native: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deleted_project_sessions WHERE agent_family=? AND provider_store=? AND native_session_id=?)")
            .bind(family.as_str()).bind(store).bind(native).fetch_one(&self.pool).await
    }

    /// Which of these sessions were deleted, in one query: a page of native
    /// rows is checked together, not one query per row.
    pub async fn project_sessions_deleted(
        &self,
        keys: &[(AgentFamily, String, String)],
    ) -> Result<HashSet<(AgentFamily, String, String)>, sqlx::Error> {
        if keys.is_empty() {
            return Ok(HashSet::new());
        }
        let keys = serde_json::to_string(&keys).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let rows = sqlx::query("WITH requested AS (SELECT json_extract(value,'$[0]') AS family,json_extract(value,'$[1]') AS store,json_extract(value,'$[2]') AS native FROM json_each(?)) SELECT k.family,k.store,k.native FROM requested k JOIN deleted_project_sessions d ON d.agent_family=k.family AND d.provider_store=k.store AND d.native_session_id=k.native")
            .bind(keys).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok((
                    AgentFamily::from_storage(row.get::<&str, _>("family"))?,
                    row.get::<String, _>("store"),
                    row.get::<String, _>("native"),
                ))
            })
            .collect()
    }

    /// Serialize the work check, tombstones and content removal with enqueue and
    /// history writers. A repeated request succeeds after a lost response.
    pub async fn delete_project_chat(
        &self,
        id: &str,
        now: &str,
    ) -> Result<DeleteProjectChat, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM project_conversations WHERE conversation_id=?)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            let deleted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=?)",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            return Ok(if deleted {
                DeleteProjectChat::Deleted
            } else {
                DeleteProjectChat::Missing
            });
        }
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND state IN ('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming','uncertain')) OR EXISTS(SELECT 1 FROM approvals WHERE (conversation_id=?1 OR thread_id IN (SELECT runtime_thread_id FROM runtime_bindings WHERE conversation_id=?1)) AND state IN ('pending','resolving')) OR EXISTS(SELECT 1 FROM automation_runs WHERE conversation_id=?1 AND status='running') OR EXISTS(SELECT 1 FROM async_questions WHERE conversation_id=?1 AND state='pending' AND expires_at_ms>unixepoch()*1000) OR EXISTS(SELECT 1 FROM computer_sessions WHERE conversation_id=?1 AND state IN ('preparing','awaitingSource','live','paused','stale'))")
            .bind(id).fetch_one(&mut *tx).await?;
        if busy {
            return Ok(DeleteProjectChat::Working);
        }

        sqlx::query("INSERT INTO deleted_project_chats VALUES(?,?)")
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        for table in ["project_conversations", "project_runtime_history"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT OR IGNORE INTO deleted_project_sessions SELECT agent_family,provider_store,native_session_id FROM {table} WHERE conversation_id=? AND native_session_id IS NOT NULL")))
                .bind(id).execute(&mut *tx).await?;
        }
        // Buffered provider notifications have no conversation FK. Drop only
        // this chat's known runtime threads, before removing their bindings.
        for table in [
            "pending_app_server_notifications",
            "terminal_approval_turns",
            "update_turn_settings",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table} WHERE thread_id IN (SELECT runtime_thread_id FROM runtime_bindings WHERE conversation_id=?)")))
                .bind(id).execute(&mut *tx).await?;
        }
        // Children are independent chats. Detach their attribution; never delete
        // their history, runtime or project files along with the parent.
        sqlx::query(
            "DELETE FROM subagent_ownership WHERE conversation_id=? OR parent_conversation_id=?",
        )
        .bind(id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        // Ledger payloads can contain the old chat text. Purge its events too;
        // the host announces a fresh catalog invalidation after committing.
        sqlx::query("DELETE FROM events WHERE json_extract(payload_json,'$.conversationId')=? OR json_extract(payload_json,'$.conversation_id')=?")
            .bind(id).bind(id).execute(&mut *tx).await?;
        for table in [
            "push_outbox",
            "push_routes",
            "push_intents",
            "computer_control_leases",
            "computer_sessions",
            "automation_runs",
            "automations",
            "async_questions",
            "approvals",
            "messages",
            "assistant_messages",
            "conversation_files",
            "history_entries",
            "history_hydration",
            "sync_journal",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE conversation_id=?"
            )))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE push_presence SET conversation_id=NULL WHERE conversation_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // Remaining FK-owned settings, delegations and provider mappings cascade.
        sqlx::query("DELETE FROM conversations WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // Deletion triggers mark search stale, so clear it after the content.
        for table in [
            "search_documents",
            "search_document_owners",
            "search_stale_conversations",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE conversation_id=?"
            )))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(DeleteProjectChat::Deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed(store: &Store, id: &str, native: &str) {
        sqlx::query("INSERT OR IGNORE INTO projects(id,name,primary_root_id,created_at,updated_at) VALUES('p','Project','root','now','now')")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,created_at) VALUES(?,'now')")
            .bind(id)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,native_session_id,cwd,roots_revision,title,created_at,updated_at,last_activity_at) VALUES(?,'p','codex','home',?,'/project',1,'Saved title','now','now','now')")
            .bind(id).bind(native).execute(&store.pool).await.unwrap();
    }

    // Contract: deletion is atomic, work-fenced, durable and idempotent. It
    // erases only the requested chat, including switched provider identities,
    // and an old discovery/hydration cannot recreate it after restart.
    #[tokio::test]
    async fn deletion_preserves_other_chats_and_blocks_rediscovery_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        seed(&store, "victim", "native").await;
        seed(&store, "child", "child-native").await;
        sqlx::query("INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES('d','Test','owner','{}','now')").execute(&store.pool).await.unwrap();
        for id in ["victim", "child"] {
            sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body_sha256,conversation_id,state,created_at,body) VALUES(?,'d',?,'hash',?,'completed','now','Saved text')")
                .bind(id).bind(id).bind(id).execute(&store.pool).await.unwrap();
            sqlx::query("INSERT INTO conversation_files(id,conversation_id,kind,name,relative_path,state,created_at,updated_at) VALUES(?,?,'artifact','Report','report.txt','ready','now','now')")
                .bind(id).bind(id).execute(&store.pool).await.unwrap();
        }
        sqlx::query("INSERT INTO events(event_id,host_epoch,sequence,occurred_at,payload_json) VALUES('deleted-event','epoch',1,'now','{\"conversationId\":\"victim\",\"event\":{\"type\":\"activity\"}}')").execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO search_documents(kind,result_id,title,body,conversation_id,updated_at) VALUES('message','victim','Saved title','Saved text','victim','now')").execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO subagent_ownership(conversation_id,parent_conversation_id,thread_id,parent_thread_id,source_json,verified_at) VALUES('child','victim','child-native','native','{}','now')")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO project_runtime_history(conversation_id,agent_family,provider_store,native_session_id,switched_at) VALUES('victim','claude','claude-home','old-native','now')")
            .execute(&store.pool).await.unwrap();
        sqlx::query("UPDATE messages SET state='uncertain' WHERE id='victim'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.delete_project_chat("victim", "now").await.unwrap(),
            DeleteProjectChat::Working
        );
        assert!(!store
            .project_session_deleted(AgentFamily::Codex, "home", "native")
            .await
            .unwrap());
        sqlx::query("UPDATE messages SET state='completed' WHERE id='victim'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER refuse_delete BEFORE DELETE ON conversation_files BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END")
            .execute(&store.pool).await.unwrap();
        assert!(store.delete_project_chat("victim", "now").await.is_err());
        assert!(store
            .project_conversation("victim")
            .await
            .unwrap()
            .is_some());
        assert!(!store
            .project_session_deleted(AgentFamily::Codex, "home", "native")
            .await
            .unwrap());
        sqlx::query("DROP TRIGGER refuse_delete")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.delete_project_chat("victim", "now").await.unwrap(),
            DeleteProjectChat::Deleted
        );
        for table in [
            "conversations",
            "project_conversations",
            "messages",
            "conversation_files",
            "history_entries",
            "search_stale_conversations",
        ] {
            let column = if table == "conversations" {
                "id"
            } else {
                "conversation_id"
            };
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE {column}='victim'"
            )))
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        let invalidation: (String, Option<String>) = sqlx::query_as("SELECT change_kind,payload_json FROM sync_journal WHERE conversation_id='victim' AND resource='conversations' ORDER BY sequence DESC LIMIT 1").fetch_one(&store.pool).await.unwrap();
        assert_eq!(invalidation, ("delete".into(), None));
        let remaining_events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE event_id='deleted-event'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(remaining_events, 0);
        let indexed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_documents WHERE conversation_id='victim'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(indexed, 0);
        assert!(store.project_conversation("child").await.unwrap().is_some());
        assert!(store.message_by_id("child").await.unwrap().is_some());
        assert_eq!(
            store.list_conversation_files("child").await.unwrap().len(),
            1
        );
        assert_eq!(
            store.delete_project_chat("victim", "later").await.unwrap(),
            DeleteProjectChat::Deleted
        );
        assert_eq!(
            store.delete_project_chat("unknown", "now").await.unwrap(),
            DeleteProjectChat::Missing
        );
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        assert!(store
            .project_session_deleted(AgentFamily::Codex, "home", "native")
            .await
            .unwrap());
        assert!(store
            .project_session_deleted(AgentFamily::Claude, "claude-home", "old-native")
            .await
            .unwrap());
        assert!(!store
            .project_session_deleted(AgentFamily::Codex, "different-home", "native")
            .await
            .unwrap());
        // The batched check answers the same, in one query, for any mix of rows.
        let key = |family, store: &str, native: &str| (family, store.to_owned(), native.to_owned());
        let asked = [
            key(AgentFamily::Codex, "home", "native"),
            key(AgentFamily::Codex, "different-home", "native"),
            key(AgentFamily::Claude, "claude-home", "old-native"),
            key(AgentFamily::Claude, "claude-home", "never-deleted"),
        ];
        assert_eq!(
            store.project_sessions_deleted(&asked).await.unwrap(),
            HashSet::from([asked[0].clone(), asked[2].clone()])
        );
        assert!(store
            .project_sessions_deleted(&[])
            .await
            .unwrap()
            .is_empty());
        assert!(
            sqlx::query("INSERT INTO conversations(id,created_at) VALUES('victim','later')")
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(sqlx::query("INSERT INTO history_hydration(conversation_id,state,updated_at_ms) VALUES('victim','ready',1)").execute(&store.pool).await.is_err());
        sqlx::query("INSERT INTO conversations(id,created_at) VALUES('rediscovered','later')")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(sqlx::query("INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,native_session_id,cwd,roots_revision,title,created_at,updated_at,last_activity_at) VALUES('rediscovered','p','codex','home','native','/project',1,'Returned','now','now','now')").execute(&store.pool).await.is_err());
    }
}
