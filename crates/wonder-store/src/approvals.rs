//! Terminal-turn consent: a finished turn can no longer consume a response.
use super::*;

impl Store {
    /// Native reset closes prompts from the old transcript in the binding's
    /// transaction. The idempotency key and body hash stay for duplicate
    /// detection; the saved answer is cleared, as on every resolve, and is
    /// never a new authorization against the replacement native session.
    pub(crate) async fn retire_reset_inputs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        thread: &str,
        retained_turns: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO terminal_approval_turns(runtime_id,thread_id,turn_id) SELECT json_extract(params_json,'$._wonderRuntimeId'),thread_id,turn_id FROM approvals WHERE thread_id=? AND state IN ('pending','resolving') AND turn_id NOT IN (SELECT value FROM json_each(?)) AND json_extract(params_json,'$._wonderRuntimeId') IS NOT NULL ON CONFLICT DO NOTHING")
            .bind(thread).bind(retained_turns).execute(&mut **tx).await?;
        sqlx::query("UPDATE approvals SET state='resolved',decision='conversation_reset',resolved_at=?,resolution_json=NULL WHERE thread_id=? AND state IN ('pending','resolving') AND turn_id NOT IN (SELECT value FROM json_each(?))")
            .bind(now).bind(thread).bind(retained_turns).execute(&mut **tx).await?;
        sqlx::query("UPDATE async_questions SET state='expired' WHERE thread_id=? AND state='pending' AND turn_id NOT IN (SELECT value FROM json_each(?))")
            .bind(thread).bind(retained_turns).execute(&mut **tx).await?;
        Ok(())
    }

    /// The requests still open (`pending` or `resolving`) whose server
    /// request ID starts with `prefix`, with their state: one asker's
    /// questions (wonderd's `t3_code::questions` keys them by T3 thread).
    pub async fn open_approvals_with_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT server_request_id,state FROM approvals WHERE substr(server_request_id,1,length(?1))=?1 AND state IN ('pending','resolving') ORDER BY created_at")
            .bind(prefix).fetch_all(&self.pool).await
    }

    /// The one Codex conversation whose native thread is `thread`, when
    /// ChatGPT's desktop app asked a request in its turn `turn` (wonderd's
    /// `chatgpt_approvals` keys those `chatgpt:{thread}:{turn}:{id}`): ChatGPT
    /// runs the turn, so no Cupola message or dispatch maps it.
    pub async fn chatgpt_request_conversation(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let owners: Vec<String> = sqlx::query_scalar("SELECT c.conversation_id FROM project_conversations c WHERE c.native_session_id=?1 AND c.agent_family='codex' AND c.wonder_archived_at IS NULL AND EXISTS(SELECT 1 FROM approvals a WHERE a.thread_id=?1 AND a.turn_id=?2 AND substr(a.server_request_id,1,8)='chatgpt:') LIMIT 2")
            .bind(thread).bind(turn).fetch_all(&self.pool).await?;
        Ok(match owners.as_slice() {
            [owner] => Some(owner.clone()),
            _ => None,
        })
    }

    /// Closes a request nobody is answering from the phone right now
    /// (`pending` only): a `resolving` one belongs to the answer under way.
    pub async fn retire_pending_approval(
        &self,
        server_request_id: &str,
        decision: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE approvals SET state='resolved', decision=?, resolved_at=?, resolution_json=NULL WHERE server_request_id=? AND state='pending'")
            .bind(decision).bind(now).bind(server_request_id).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// Closes a request that is `pending` or `resolving`. For a caller
    /// holding wonderd's `approval_lock`, which an answer from the phone
    /// holds throughout: a `resolving` row seen then has no answer under
    /// way (its answer's outcome is unknown, or the host restarted).
    pub async fn retire_open_approval(
        &self,
        server_request_id: &str,
        decision: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE approvals SET state='resolved', decision=?, resolved_at=?, resolution_json=NULL WHERE server_request_id=? AND state IN ('pending','resolving')")
            .bind(decision).bind(now).bind(server_request_id).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// The open requests nobody is answering from the phone right now
    /// (`pending`) whose server request ID starts with `prefix`, each with
    /// the nonce it was offered under (`retire_pending_approval_offered`).
    pub async fn pending_approvals_with_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT server_request_id,action_nonce FROM approvals WHERE substr(server_request_id,1,length(?1))=?1 AND state='pending' ORDER BY created_at")
            .bind(prefix).fetch_all(&self.pool).await
    }

    /// `retire_pending_approval`, only while the request is still the one
    /// offered under `nonce`: a request reopened since (a new nonce) was read
    /// again after the caller's read, so the caller's verdict is stale.
    pub async fn retire_pending_approval_offered(
        &self,
        server_request_id: &str,
        nonce: &str,
        decision: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE approvals SET state='resolved', decision=?, resolved_at=? WHERE server_request_id=? AND state='pending' AND action_nonce=?")
            .bind(decision).bind(now).bind(server_request_id).bind(nonce).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// Offers again a request closed only because Cupola lost its asker
    /// (`lost_decision`), now that the asker, read again, still asks it: with
    /// the params just read and a fresh nonce, so nothing signed for the
    /// closed offer applies. A request answered from the phone, being
    /// answered, or closed any other way stays as it is.
    pub async fn reopen_lost_approval(
        &self,
        server_request_id: &str,
        lost_decision: &str,
        params_json: &str,
    ) -> Result<bool, sqlx::Error> {
        let params = serde_json::from_str::<serde_json::Value>(params_json).unwrap_or_default();
        let text = |key: &str| {
            params
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let result = sqlx::query("UPDATE approvals SET state='pending', decision=NULL, resolved_at=NULL, params_json=?, thread_id=?, turn_id=?, item_id=?, action_nonce=? WHERE server_request_id=? AND state='resolved' AND decision=? AND resolution_idempotency_key IS NULL AND resolution_json IS NULL")
            .bind(params_json)
            .bind(text("threadId"))
            .bind(text("turnId"))
            .bind(text("itemId"))
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(server_request_id)
            .bind(lost_decision)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// An answer that certainly never reached the asker: the request is
    /// open again with no saved answer, so the phone can answer it anew
    /// (a new answer has a new idempotency key).
    pub async fn abandon_approval_resolution(&self, approval_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE approvals SET state='pending', resolution_idempotency_key=NULL, resolution_body_sha256=NULL, resolution_json=NULL WHERE approval_id=? AND state='resolving'")
            .bind(approval_id).execute(&self.pool).await?;
        Ok(())
    }

    /// A saved answer is needed only while its request is `resolving` (crash
    /// recovery), and may be a secret the owner typed: none outlives its
    /// request. Clears those an older version kept after resolving.
    pub(crate) async fn forget_resolved_answers(&self) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE approvals SET resolution_json=NULL WHERE state='resolved' AND resolution_json IS NOT NULL")
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn approval_turn_ended(
        &self,
        runtime: &str,
        thread: &str,
        turn: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM terminal_approval_turns WHERE runtime_id=? AND thread_id=? AND turn_id=?)")
            .bind(runtime).bind(thread).bind(turn).fetch_one(&self.pool).await
    }

    /// Persist the terminal identity, retire at most 256 requests, and journal
    /// their phone events in one commit. A saved answer is cleared (it may be
    /// a secret) and is never retried against a turn that has already ended.
    pub async fn retire_turn_approvals(
        &self,
        runtime: &str,
        thread: &str,
        turn: &str,
        template: &HostEventEnvelope,
    ) -> Result<Vec<HostEventEnvelope>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO terminal_approval_turns(runtime_id,thread_id,turn_id) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(runtime).bind(thread).bind(turn).execute(&mut *tx).await?;
        let ids: Vec<String> = sqlx::query_scalar("UPDATE approvals SET state='resolved', decision='turn_ended', resolved_at=?, resolution_json=NULL WHERE approval_id IN (SELECT approval_id FROM approvals WHERE thread_id=? AND turn_id=? AND json_extract(params_json,'$._wonderRuntimeId')=? AND state IN ('pending','resolving') LIMIT 256) RETURNING approval_id")
            .bind(&template.occurred_at).bind(thread).bind(turn).bind(runtime).fetch_all(&mut *tx).await?;
        let mut events = Vec::with_capacity(ids.len());
        for id in ids {
            let mut event = template.clone();
            event.event_id = uuid::Uuid::new_v4().to_string();
            event.request_id = Some(id.clone());
            event.approval_id = Some(id.clone());
            event.event = WonderEvent::ApprovalResolved {
                request_id: id,
                decision: "turn_ended".into(),
            };
            self.commit_event_in(&mut tx, &mut event).await?;
            events.push(event);
        }
        tx.commit().await?;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_approval_migration_preserves_existing_device_message_and_request() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("upgrade.db").display()
        );
        let pool = SqlitePoolOptions::new().connect(&url).await.unwrap();
        let old = sqlx::migrate::Migrator::with_migrations(
            sqlx::migrate!()
                .iter()
                .filter(|migration| migration.version < 101)
                .cloned()
                .collect(),
        );
        old.run(&pool).await.unwrap();
        let store = Store { pool };
        // Seed the historical schema directly: current write APIs may require
        // columns that this migration fixture intentionally has not added yet.
        sqlx::query("INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES('owner','Paired phone','owner','{}','now')")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) VALUES('message','owner','saved-draft','Keep this work','hash','chat','accepted_by_wonder','now')")
            .execute(&store.pool).await.unwrap();
        let message = StoredMessage {
            id: "message".into(),
            device_id: "owner".into(),
            client_message_id: "saved-draft".into(),
            body: "Keep this work".into(),
            body_sha256: "hash".into(),
            conversation_id: "chat".into(),
            state: "accepted_by_wonder".into(),
            created_at: "now".into(),
            codex_thread_id: None,
            codex_turn_id: None,
        };
        let params = r#"{"_wonderRuntimeId":"runtime","threadId":"thread","turnId":"turn"}"#;
        sqlx::query("INSERT INTO approvals(approval_id,server_request_id,method,params_json,thread_id,turn_id,action_nonce,state,created_at) VALUES('pending','pending','item/fileChange/requestApproval',?,'thread','turn','original-nonce','pending','now')")
            .bind(params).execute(&store.pool).await.unwrap();
        store.pool.close().await;
        let upgraded = Store::connect(&url).await.unwrap();
        assert_eq!(
            upgraded.message_by_id(&message.id).await.unwrap().unwrap(),
            message
        );
        let device: (String, String, Option<String>) =
            sqlx::query_as("SELECT label,public_key_jwk,revoked_at FROM devices WHERE id='owner'")
                .fetch_one(&upgraded.pool)
                .await
                .unwrap();
        assert_eq!(device, ("Paired phone".into(), "{}".into(), None));
        let request = upgraded.approval("pending").await.unwrap().unwrap();
        assert_eq!(request.params_json, params);
        assert_eq!(request.action_nonce, "original-nonce");
        assert_eq!(request.state, "pending");
        assert!(!upgraded
            .approval_turn_ended("runtime", "thread", "turn")
            .await
            .unwrap());
        let integrity: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    // Contract: only a request closed because Cupola lost its asker is
    // offered again, under a fresh nonce and the asker's newest params, and a
    // verdict read before that reopen can't close it.
    #[tokio::test]
    async fn only_a_lost_request_reopens_and_a_stale_read_cannot_close_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let method = "item/tool/requestUserInput";
        let asked = |turn: &str| {
            serde_json::json!({"threadId": "thread", "turnId": turn, "itemId": "item"}).to_string()
        };
        for key in ["lost", "answered", "resolving", "there"] {
            store
                .insert_pending_approval(key, method, &asked("turn"), "now")
                .await
                .unwrap();
        }
        let first = store.approval("lost").await.unwrap().unwrap();
        let read = store.pending_approvals_with_prefix("lo").await.unwrap();
        assert_eq!(read, [("lost".to_owned(), first.action_nonce.clone())]);
        assert!(store
            .retire_pending_approval("lost", "on_computer", "later")
            .await
            .unwrap());
        assert!(store
            .retire_pending_approval("there", "resolved_there", "later")
            .await
            .unwrap());
        // Answered from the phone, as its answer is saved before it is sent.
        store.begin_approval_resolution("answered").await.unwrap();
        assert!(store
            .persist_approval_resolution_intent("answered", "resolve-1", "hash", "{}")
            .await
            .unwrap());
        store
            .finish_approval_resolution("answered", "on_computer", "later")
            .await
            .unwrap();
        store.begin_approval_resolution("resolving").await.unwrap();
        for key in ["answered", "resolving", "there"] {
            assert!(
                !store
                    .reopen_lost_approval(key, "on_computer", &asked("turn"))
                    .await
                    .unwrap(),
                "{key}"
            );
        }
        assert!(!store
            .reopen_lost_approval("lost", "resolved_there", &asked("turn"))
            .await
            .unwrap());
        assert!(store
            .reopen_lost_approval("lost", "on_computer", &asked("turn-2"))
            .await
            .unwrap());
        let reopened = store.approval("lost").await.unwrap().unwrap();
        assert_eq!(reopened.state, "pending");
        assert_eq!(reopened.decision, None);
        assert_eq!(reopened.turn_id, "turn-2");
        assert_eq!(reopened.params_json, asked("turn-2"));
        assert_ne!(reopened.action_nonce, first.action_nonce);
        assert!(
            !store
                .reopen_lost_approval("lost", "on_computer", &asked("turn-2"))
                .await
                .unwrap(),
            "an open request is not reopened"
        );
        // A verdict read under the first offer is stale now.
        assert!(!store
            .retire_pending_approval_offered("lost", &first.action_nonce, "on_computer", "later")
            .await
            .unwrap());
        assert_eq!(
            store.approval("lost").await.unwrap().unwrap().state,
            "pending"
        );
        assert!(store
            .retire_pending_approval_offered("lost", &reopened.action_nonce, "on_computer", "later")
            .await
            .unwrap());
        for (key, state) in [
            ("answered", "resolved"),
            ("resolving", "resolving"),
            ("there", "resolved"),
        ] {
            assert_eq!(
                store.approval(key).await.unwrap().unwrap().state,
                state,
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn terminal_approval_commit_is_atomic_bounded_and_prevents_late_insert() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let params =
            serde_json::json!({"_wonderRuntimeId":"runtime","threadId":"thread","turnId":"turn"})
                .to_string();
        for i in 0..257 {
            store
                .insert_pending_approval(
                    &format!("{i}"),
                    "item/fileChange/requestApproval",
                    &params,
                    "now",
                )
                .await
                .unwrap();
        }
        let template = HostEventEnvelope {
            event_id: String::new(),
            host_epoch: "epoch".into(),
            sequence: 0,
            occurred_at: "2026-10-13T00:00:00Z".into(),
            request_id: None,
            device_id: None,
            conversation_id: None,
            message_id: None,
            thread_id: Some("thread".into()),
            turn_id: Some("turn".into()),
            item_id: None,
            approval_id: None,
            event: WonderEvent::ApprovalResolved {
                request_id: String::new(),
                decision: "turn_ended".into(),
            },
        };
        // No active epoch: event commit fails and the marker/approvals roll back.
        assert!(store
            .retire_turn_approvals("runtime", "thread", "turn", &template)
            .await
            .is_err());
        assert!(!store
            .approval_turn_ended("runtime", "thread", "turn")
            .await
            .unwrap());
        assert_eq!(store.list_pending_approvals().await.unwrap().len(), 257);
        store.start_event_epoch("epoch").await.unwrap();
        let first = store
            .retire_turn_approvals("runtime", "thread", "turn", &template)
            .await
            .unwrap();
        assert_eq!(first.len(), 256);
        assert_eq!(store.list_pending_approvals().await.unwrap().len(), 1);
        // A crash between batches cannot let the remaining row forward an
        // answer, including an answer whose write was previously interrupted.
        let remaining = store.list_pending_approvals().await.unwrap().pop().unwrap();
        assert!(store
            .begin_approval_resolution(&remaining.approval_id)
            .await
            .unwrap()
            .is_none());
        sqlx::query("UPDATE approvals SET state='resolving' WHERE approval_id=?")
            .bind(&remaining.approval_id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .resume_approval_resolution(&remaining.approval_id)
            .await
            .unwrap()
            .is_none());

        assert_eq!(
            store
                .retire_turn_approvals("runtime", "thread", "turn", &template)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .retire_turn_approvals("runtime", "thread", "turn", &template)
            .await
            .unwrap()
            .is_empty());
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(events, 257);
        store
            .insert_pending_approval("late", "item/fileChange/requestApproval", &params, "now")
            .await
            .unwrap();
        assert!(store.approval("late").await.unwrap().is_none());
        let plan: Vec<sqlx::sqlite::SqliteRow> = sqlx::query("EXPLAIN QUERY PLAN SELECT approval_id FROM approvals WHERE thread_id=? AND turn_id=? AND json_extract(params_json,'$._wonderRuntimeId')=? AND state IN ('pending','resolving') LIMIT 256")
            .bind("thread").bind("turn").bind("runtime").fetch_all(&store.pool).await.unwrap();
        assert!(
            plan.iter().any(|row| row
                .get::<String, _>("detail")
                .contains("approvals_pending_turn")),
            "retirement must use the exact-turn index"
        );
    }

    // A secret typed into a question is kept only while its answer is on its
    // way: every way a request closes clears it, and so does an upgrade.
    #[tokio::test]
    async fn resolving_clears_a_saved_answer_but_keeps_duplicate_detection() {
        const SECRET: &str = "sk-synthetic-8f3a1c2e9b7d";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Store::connect(&url).await.unwrap();
        let ids = ["finished", "by-request", "retired", "turn-ended", "reset"];
        for id in ids {
            let params =
                serde_json::json!({"_wonderRuntimeId":"runtime","threadId":"thread","turnId":id})
                    .to_string();
            store
                .insert_pending_approval(id, "item/tool/requestUserInput", &params, "now")
                .await
                .unwrap();
            if id == "retired" {
                // Only a pending one is retired; it can still hold an answer
                // an older version saved.
                sqlx::query("UPDATE approvals SET resolution_idempotency_key='key',resolution_body_sha256='hash',resolution_json=? WHERE approval_id=?")
                    .bind(format!(r#"{{"answers":{{"q":{{"answers":["{SECRET}"]}}}}}}"#)).bind(id).execute(&store.pool).await.unwrap();
                continue;
            }
            store.begin_approval_resolution(id).await.unwrap().unwrap();
            assert!(store
                .persist_approval_resolution_intent(
                    id,
                    "key",
                    "hash",
                    &format!(r#"{{"answers":{{"q":{{"answers":["{SECRET}"]}}}}}}"#),
                )
                .await
                .unwrap());
        }
        store
            .finish_approval_resolution("finished", "respond", "now")
            .await
            .unwrap();
        assert!(store
            .resolve_approval_by_server_request_id("by-request", "respond", "now")
            .await
            .unwrap());
        assert!(store
            .retire_pending_approval("retired", "on_computer", "now")
            .await
            .unwrap());
        let template = HostEventEnvelope {
            event_id: String::new(),
            host_epoch: "epoch".into(),
            sequence: 0,
            occurred_at: "now".into(),
            request_id: None,
            device_id: None,
            conversation_id: None,
            message_id: None,
            thread_id: Some("thread".into()),
            turn_id: Some("turn-ended".into()),
            item_id: None,
            approval_id: None,
            event: WonderEvent::ApprovalResolved {
                request_id: String::new(),
                decision: String::new(),
            },
        };
        store.start_event_epoch("epoch").await.unwrap();
        store
            .retire_turn_approvals("runtime", "thread", "turn-ended", &template)
            .await
            .unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        store
            .retire_reset_inputs(
                &mut tx,
                "thread",
                r#"["finished","by-request","retired","turn-ended"]"#,
                "now",
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        for id in ids {
            let approval = store.approval(id).await.unwrap().unwrap();
            assert_eq!(approval.state, "resolved", "{id}");
            assert_eq!(approval.resolution_json, None, "{id}");
            assert_eq!(
                approval.resolution_idempotency_key.as_deref(),
                Some("key"),
                "{id}"
            );
            assert_eq!(
                approval.resolution_body_sha256.as_deref(),
                Some("hash"),
                "{id}"
            );
        }
        // An answer an older version left on a resolved request.
        sqlx::query("UPDATE approvals SET resolution_json=? WHERE approval_id='finished'")
            .bind(SECRET)
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        let upgraded = Store::connect(&url).await.unwrap();
        assert_eq!(
            upgraded
                .approval("finished")
                .await
                .unwrap()
                .unwrap()
                .resolution_json,
            None
        );
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&upgraded.pool)
            .await
            .unwrap();
        upgraded.pool.close().await;
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes()),
            "the cleared answer must not remain in the database file"
        );
    }
}
