//! Moving a Project thread to the other provider. A thread's provider is
//! otherwise fixed (see `runtime_bindings.rs` and the migration 0095
//! triggers); `switch_project_runtime` is the only write that may change it,
//! and the host calls it only while releasing a message that carries a model
//! from the other provider.

use super::*;

/// What a switch asks for. Title, folder, history and queue stay. Access,
/// approval and plan mode follow what the new provider accepts.
#[derive(Clone, Debug)]
pub struct ProjectRuntimeSwitch<'a> {
    pub conversation_id: &'a str,
    pub family: AgentFamily,
    pub provider_store: &'a str,
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub service_tier: Option<&'a str>,
    /// Readable history entries the thread has now, when the caller counted them.
    pub last_position: Option<i64>,
    pub now: &'a str,
    /// The message being released: it is the work that asked for the switch.
    pub except_message: Option<&'a str>,
    /// Continue this earlier session of the target provider (a history entry
    /// of that provider) instead of starting a fresh one. With the thread's
    /// own provider as target and no entry, the current session is replaced.
    pub resume: Option<i64>,
}

/// A native session a thread left behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeHistoryEntry {
    pub id: i64,
    pub family: AgentFamily,
    pub provider_store: String,
    pub native_session_id: Option<String>,
    pub runtime_thread_id: Option<String>,
    pub model: Option<String>,
    pub switched_at: String,
    pub last_position: Option<i64>,
    /// Set while the session's history still has to be imported: the
    /// timeline position (ms) its items must stay before.
    pub history_import_before_ms: Option<i64>,
}

/// The context still owed to the thread's next turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingHandoff {
    /// The session that was left when the switch happened.
    pub history_id: i64,
    /// A resumed session needs only what happened after this entry.
    pub delta_after_history_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffDelivery {
    Pending,
    Injected,
}

/// The model one message carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageTarget {
    /// `None` for messages accepted before the host recorded it.
    pub family: Option<AgentFamily>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}

fn busy() -> sqlx::Error {
    sqlx::Error::Protocol("provider_switch_busy".into())
}

fn history_entry(row: &sqlx::sqlite::SqliteRow) -> Result<RuntimeHistoryEntry, sqlx::Error> {
    Ok(RuntimeHistoryEntry {
        id: row.get("id"),
        family: AgentFamily::from_storage(row.get("agent_family"))?,
        provider_store: row.get("provider_store"),
        native_session_id: row.get("native_session_id"),
        runtime_thread_id: row.get("runtime_thread_id"),
        model: row.get("model"),
        switched_at: row.get("switched_at"),
        last_position: row.get("last_position"),
        history_import_before_ms: row.get("history_import_before_ms"),
    })
}

impl Store {
    /// Commit Claude's own session rotation, without injecting a context handoff.
    /// Exact old/new identity makes replay after a lost acknowledgement idempotent.
    pub async fn reset_project_native_session(
        &self,
        thread: &str,
        old: &str,
        new: &str,
        retained_turns: &[String],
        now: &str,
    ) -> Result<(String, i64), sqlx::Error> {
        if old == new || uuid::Uuid::parse_str(old).is_err() || uuid::Uuid::parse_str(new).is_err()
        {
            return Err(sqlx::Error::Protocol(
                "Invalid native reset identity".into(),
            ));
        }
        if retained_turns.len() > 200
            || retained_turns
                .iter()
                .any(|turn| turn.is_empty() || turn.len() > 128)
        {
            return Err(sqlx::Error::Protocol(
                "Invalid retained native turns".into(),
            ));
        }
        let retained =
            serde_json::to_string(retained_turns).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT p.conversation_id,p.native_session_id,c.history_revision,c.last_native_reset_session_id,b.session_id FROM project_conversations p JOIN runtime_bindings b USING(conversation_id) JOIN conversations c ON c.id=p.conversation_id WHERE b.runtime_thread_id=? AND b.agent_family='claude' AND p.agent_family='claude' AND b.execution_scope='projects' AND b.provider_store=p.provider_store")
            .bind(thread).fetch_optional(&mut *tx).await?
            .ok_or_else(|| sqlx::Error::Protocol("Native reset has no owned Claude binding".into()))?;
        let conversation: String = row.get("conversation_id");
        let native: Option<String> = row.get("native_session_id");
        let binding: Option<String> = row.get("session_id");
        let revision: i64 = row.get("history_revision");
        if native.as_deref() == Some(new)
            && binding.as_deref() == Some(new)
            && revision > 0
            && row
                .get::<Option<String>, _>("last_native_reset_session_id")
                .as_deref()
                == Some(old)
        {
            tx.commit().await?;
            return Ok((conversation, revision));
        }
        if native.as_deref() != Some(old) || binding.as_deref() != Some(old) {
            return Err(sqlx::Error::Protocol("Native reset binding changed".into()));
        }
        let unowned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM json_each(?) t WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id=? AND m.codex_thread_id=? AND m.codex_turn_id=t.value)")
            .bind(&retained).bind(&conversation).bind(thread).fetch_one(&mut *tx).await?;
        if unowned != 0 {
            return Err(sqlx::Error::Protocol(
                "Retained native turn is not owned".into(),
            ));
        }
        sqlx::query("INSERT INTO project_runtime_switch_guard(conversation_id) VALUES(?)")
            .bind(&conversation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE project_conversations SET native_session_id=?,native_settings_turn=NULL,updated_at=? WHERE conversation_id=?")
            .bind(new).bind(now).bind(&conversation).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE runtime_bindings SET session_id=?,updated_at=? WHERE conversation_id=?",
        )
        .bind(new)
        .bind(now)
        .bind(&conversation)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE conversations SET session_id=?,last_native_reset_session_id=?,history_revision=history_revision+1 WHERE id=?")
            .bind(new).bind(old).bind(&conversation).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM project_runtime_switch_guard WHERE conversation_id=?")
            .bind(&conversation)
            .execute(&mut *tx)
            .await?;
        // Messages queued in Wonder have not entered Claude's old context.
        sqlx::query("UPDATE history_entries SET hidden_by_reset=1 WHERE conversation_id=? AND NOT (source='message' AND source_id IN (SELECT id FROM messages WHERE conversation_id=? AND ((codex_turn_id IS NULL AND state IN ('accepted_by_wonder','safe_to_retry','failed')) OR codex_turn_id IN (SELECT value FROM json_each(?)))))")
            .bind(&conversation).bind(&conversation).bind(&retained).execute(&mut *tx).await?;
        // An acknowledged native clear ends the old context even if a crash
        // lost its terminal notification. Preserve receipts while releasing
        // stale delivery/uncertainty gates; unsent queue and retained Guide stay.
        sqlx::query(sql!("UPDATE messages SET state='interrupted' WHERE conversation_id=? AND state IN ({}) AND COALESCE(codex_turn_id,'') NOT IN (SELECT value FROM json_each(?))", turn_states_sql(TurnScope::Unsettled)))
            .bind(&conversation).bind(&retained).execute(&mut *tx).await?;
        sqlx::query("UPDATE dispatch_attempts SET phase='interrupted' WHERE message_id IN (SELECT m.id FROM messages m JOIN history_entries h ON h.source='message' AND h.source_id=m.id WHERE m.conversation_id=? AND h.hidden_by_reset=1 AND m.state='interrupted') AND phase IN ('dispatching_to_codex','accepted_by_codex','streaming','uncertain')")
            .bind(&conversation).execute(&mut *tx).await?;
        self.retire_reset_inputs(&mut tx, thread, &retained, now)
            .await?;
        sqlx::query("UPDATE history_hydration SET state='idle',token=NULL,file_change_version=0 WHERE conversation_id=?")
            .bind(&conversation).execute(&mut *tx).await?;
        // A provider switch handoff must never re-inject the cleared transcript.
        sqlx::query("DELETE FROM project_pending_handoff WHERE conversation_id=?")
            .bind(&conversation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((conversation, revision + 1))
    }

    /// Changes the provider in one transaction: records the session being left,
    /// then either binds the target provider's earlier session again or clears
    /// the binding so the next turn starts a fresh one; sets the family, model
    /// and settings; and marks a handoff pending when a session was left.
    /// Returns `None` for an unknown thread.
    ///
    /// Work already delivered or uncertain, other than `except_message`, makes
    /// this fail with `provider_switch_busy`: that turn belongs to the old
    /// session. Queued messages do not: they wait their turn behind this one.
    pub async fn switch_project_runtime(
        &self,
        switch: ProjectRuntimeSwitch<'_>,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        let id = switch.conversation_id;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(row) = sqlx::query("SELECT * FROM project_conversations WHERE conversation_id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(None);
        };
        let current = projects::project_conversation(&row)?;
        if !switch.family.model_in_own_namespace(switch.model)
            || switch.family.model_in_foreign_namespace(switch.model)
        {
            return Err(sqlx::Error::Protocol(
                "Choose a model from the new agent family".into(),
            ));
        }
        let working: i64 = sqlx::query_scalar(sql!("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND id<>? AND state IN ({}))", turn_states_sql(TurnScope::Unsettled)))
            .bind(id)
            .bind(switch.except_message.unwrap_or_default())
            .fetch_one(&mut *tx)
            .await?;
        if working != 0 {
            return Err(busy());
        }
        let resumed = match switch.resume {
            None => None,
            Some(entry) => {
                let found = sqlx::query(
                    "SELECT * FROM project_runtime_history WHERE id=? AND conversation_id=?",
                )
                .bind(entry)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .map(|row| history_entry(&row))
                .transpose()?;
                match found {
                    Some(found)
                        if found.family == switch.family
                            && found.native_session_id.is_some()
                            && found.runtime_thread_id.is_some() =>
                    {
                        Some(found)
                    }
                    _ => return Err(sqlx::Error::Protocol("resume_unavailable".into())),
                }
            }
        };
        let binding = sqlx::query(
            "SELECT runtime_thread_id,session_id FROM runtime_bindings WHERE conversation_id=?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let runtime_thread: Option<String> = binding.as_ref().map(|b| b.get("runtime_thread_id"));
        let left = if current.native_session_id.is_some() || runtime_thread.is_some() {
            // The left session's history is owed to the timeline until an
            // import finishes; its items stay before the switch row, which
            // is placed at `switched_at`.
            let history_id = sqlx::query("INSERT INTO project_runtime_history(conversation_id,agent_family,provider_store,native_session_id,runtime_thread_id,model,switched_at,last_position,history_import_before_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,CASE WHEN ?5 IS NULL THEN NULL WHEN ?7 NOT GLOB '*[^0-9]*' AND ?7 != '' THEN CAST(?7 AS INTEGER) ELSE COALESCE(CAST((julianday(?7) - 2440587.5) * 86400000 AS INTEGER), CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)) END)")
                .bind(id)
                .bind(current.family.as_str())
                .bind(&current.provider_store)
                .bind(&current.native_session_id)
                .bind(&runtime_thread)
                .bind(&current.model)
                .bind(switch.now)
                .bind(switch.last_position)
                .execute(&mut *tx)
                .await?
                .last_insert_rowid();
            sqlx::query("INSERT INTO project_pending_handoff(conversation_id,history_id,delta_after_history_id,created_at) VALUES(?,?,?,?) ON CONFLICT(conversation_id) DO UPDATE SET history_id=excluded.history_id,delta_after_history_id=excluded.delta_after_history_id,created_at=excluded.created_at")
                .bind(id)
                .bind(history_id)
                .bind(resumed.as_ref().map(|entry| entry.id))
                .bind(switch.now)
                .execute(&mut *tx)
                .await?;
            Some(history_id)
        } else {
            None
        };
        // A history read started before the switch must not save the left
        // session's items after it, placed after the switch row: end its
        // claim, as a native reset does. What it had not saved stays owed.
        sqlx::query("UPDATE history_hydration SET state='idle',token=NULL WHERE conversation_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO project_runtime_switch_guard(conversation_id) VALUES(?)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM runtime_bindings WHERE conversation_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE conversations SET codex_thread_id=NULL,session_id=NULL WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let provider = switch.family.provider();
        let access = if provider.capabilities.access_choices {
            current.access_mode.as_str()
        } else {
            "workspace"
        };
        sqlx::query("UPDATE project_conversations SET agent_family=?,provider_store=?,native_session_id=NULL,native_settings_turn=NULL,model=?,effort=?,service_tier=?,claude_approval=?,plan_mode=?,access_mode=?,updated_at=? WHERE conversation_id=?")
            .bind(switch.family.as_str())
            .bind(switch.provider_store)
            .bind(switch.model)
            .bind(switch.effort)
            .bind(switch.service_tier)
            .bind(provider.default_approval)
            .bind(i64::from(current.plan_mode && provider.capabilities.plan_mode()))
            .bind(access)
            .bind(switch.now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if let Some(entry) = &resumed {
            let (thread, session) = (
                entry.runtime_thread_id.as_deref().unwrap_or_default(),
                entry.native_session_id.as_deref().unwrap_or_default(),
            );
            let bound = async {
                sqlx::query("UPDATE conversations SET codex_thread_id=?,session_id=? WHERE id=?")
                    .bind(thread)
                    .bind(session)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO runtime_bindings(conversation_id,agent_family,runtime_thread_id,session_id,updated_at,execution_scope,provider_store) VALUES(?,?,?,?,?,'projects',?)")
                    .bind(id)
                    .bind(switch.family.as_str())
                    .bind(thread)
                    .bind(session)
                    .bind(switch.now)
                    .bind(switch.provider_store)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE project_conversations SET native_session_id=? WHERE conversation_id=?")
                    .bind(session)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM project_runtime_history WHERE id=?")
                    .bind(entry.id)
                    .execute(&mut *tx)
                    .await?;
                Ok::<(), sqlx::Error>(())
            }
            .await;
            // Another thread may own the session now; the caller starts fresh.
            if bound.is_err() {
                return Err(sqlx::Error::Protocol("resume_unavailable".into()));
            }
        }
        sqlx::query("DELETE FROM project_runtime_switch_guard WHERE conversation_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let _ = left;
        tx.commit().await?;
        self.project_conversation(id).await
    }

    /// Sessions the thread left, oldest first.
    pub async fn project_runtime_history(
        &self,
        conversation: &str,
    ) -> Result<Vec<RuntimeHistoryEntry>, sqlx::Error> {
        sqlx::query("SELECT * FROM project_runtime_history WHERE conversation_id=? ORDER BY id")
            .bind(conversation)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(history_entry)
            .collect()
    }

    /// Sessions the thread left whose history is still owed to its
    /// timeline, oldest first.
    pub async fn history_imports_owed(
        &self,
        conversation: &str,
    ) -> Result<Vec<RuntimeHistoryEntry>, sqlx::Error> {
        sqlx::query("SELECT * FROM project_runtime_history WHERE conversation_id=? AND history_import_before_ms IS NOT NULL AND runtime_thread_id IS NOT NULL ORDER BY id")
            .bind(conversation)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(history_entry)
            .collect()
    }

    /// The left session's history is in the timeline. Idempotent.
    pub async fn finish_history_import(
        &self,
        conversation: &str,
        history_id: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE project_runtime_history SET history_import_before_ms=NULL WHERE id=? AND conversation_id=?")
            .bind(history_id)
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The thread that switched away from this provider session and keeps it
    /// as its earlier history, if any.
    pub async fn project_conversation_by_left_session(
        &self,
        family: AgentFamily,
        store: &str,
        native: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        sqlx::query("SELECT c.* FROM project_runtime_history h JOIN project_conversations c ON c.conversation_id=h.conversation_id WHERE h.agent_family=? AND h.provider_store=? AND h.native_session_id=? ORDER BY h.id DESC LIMIT 1")
            .bind(family.as_str())
            .bind(store)
            .bind(native)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(projects::project_conversation)
            .transpose()
    }

    /// The context the thread's next turn must carry, if a switch left one.
    pub async fn pending_handoff(
        &self,
        conversation: &str,
    ) -> Result<Option<PendingHandoff>, sqlx::Error> {
        let row = sqlx::query("SELECT history_id,delta_after_history_id FROM project_pending_handoff WHERE conversation_id=?")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| PendingHandoff {
            history_id: row.get("history_id"),
            delta_after_history_id: row.get("delta_after_history_id"),
        }))
    }

    /// Idempotent: clearing a handoff that is already gone changes nothing.
    pub async fn clear_pending_handoff(&self, conversation: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM project_pending_handoff WHERE conversation_id=?")
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn handoff_delivery(
        &self,
        conversation: &str,
        thread: &str,
    ) -> Result<Option<HandoffDelivery>, sqlx::Error> {
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM project_handoff_delivery WHERE conversation_id=? AND native_thread_id=?")
            .bind(conversation)
            .bind(thread)
            .fetch_optional(&self.pool)
            .await?;
        Ok(status.map(|status| {
            if status == "injected" {
                HandoffDelivery::Injected
            } else {
                HandoffDelivery::Pending
            }
        }))
    }

    /// Written `Pending` before the turn is submitted and `Injected` after the
    /// provider accepted it; `None` forgets a delivery known not to have happened.
    pub async fn set_handoff_delivery(
        &self,
        conversation: &str,
        thread: &str,
        status: Option<HandoffDelivery>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        match status {
            None => {
                sqlx::query("DELETE FROM project_handoff_delivery WHERE conversation_id=? AND native_thread_id=?")
                    .bind(conversation)
                    .bind(thread)
                    .execute(&self.pool)
                    .await?;
            }
            Some(status) => {
                let text = match status {
                    HandoffDelivery::Pending => "pending",
                    HandoffDelivery::Injected => "injected",
                };
                sqlx::query("INSERT INTO project_handoff_delivery(conversation_id,native_thread_id,status,updated_at) VALUES(?,?,?,?) ON CONFLICT(conversation_id,native_thread_id) DO UPDATE SET status=excluded.status,updated_at=excluded.updated_at")
                    .bind(conversation)
                    .bind(thread)
                    .bind(text)
                    .bind(now)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Records the model the next accepted message from this device carries.
    /// Consumed by that message's acceptance; a retry stages it again.
    #[allow(clippy::too_many_arguments)]
    pub async fn stage_project_message_target(
        &self,
        device: &str,
        client_message: &str,
        family: AgentFamily,
        model: &str,
        effort: Option<&str>,
        service_tier: Option<&str>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        // Rows left by a send that never completed are not kept.
        sqlx::query("DELETE FROM project_message_targets WHERE created_at < datetime(?, '-1 day')")
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO project_message_targets(device_id,client_message_id,agent_family,model,effort,service_tier,created_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(device_id,client_message_id) DO UPDATE SET agent_family=excluded.agent_family,model=excluded.model,effort=excluded.effort,service_tier=excluded.service_tier,created_at=excluded.created_at")
            .bind(device)
            .bind(client_message)
            .bind(family.as_str())
            .bind(model)
            .bind(effort)
            .bind(service_tier)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn discard_project_message_target(
        &self,
        device: &str,
        client_message: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "DELETE FROM project_message_targets WHERE device_id=? AND client_message_id=?",
        )
        .bind(device)
        .bind(client_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The model, effort, speed and provider frozen with a message.
    pub async fn project_message_target(
        &self,
        message: &str,
    ) -> Result<Option<MessageTarget>, sqlx::Error> {
        let row = sqlx::query("SELECT model,reasoning_effort,service_tier,agent_family FROM message_execution_settings WHERE message_id=?")
            .bind(message)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            let family: Option<String> = row.get("agent_family");
            Ok(MessageTarget {
                family: family
                    .as_deref()
                    .map(AgentFamily::from_storage)
                    .transpose()?,
                model: row.get("model"),
                effort: row.get("reasoning_effort"),
                service_tier: row.get("service_tier"),
            })
        })
        .transpose()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) async fn thread(store: &Store, family: AgentFamily, native: Option<&str>) {
        store
            .create_project(
                "p1",
                "request-p1",
                "hash",
                "App",
                &[ProjectRootInput {
                    path: "/work/app".into(),
                    canonical_path: "/work/app".into(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "c1",
                project_id: "p1",
                family,
                provider_store: if family == AgentFamily::Codex {
                    "codex:home"
                } else {
                    "claude:home"
                },
                native_session_id: native,
                cwd: "/work/app",
                roots_revision: 1,
                title: "Fix",
                model: Some(if family == AgentFamily::Codex {
                    "gpt-5"
                } else {
                    "claude:sonnet"
                }),
                effort: Some("high"),
                service_tier: None,
                access_mode: "full_access",
                claude_approval: family.provider().default_approval,
                plan_mode: true,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn catalog_owners_batch_current_left_missing_and_other_stores() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Claude, Some("current")).await;
        for native in ["left", "current"] {
            sqlx::query("INSERT INTO project_runtime_history(conversation_id,agent_family,provider_store,native_session_id,switched_at) VALUES('c1','claude','claude:home',?,'now')")
                .bind(native).execute(&store.pool).await.unwrap();
        }
        let keys = [
            (AgentFamily::Claude, "claude:home".into(), "current".into()),
            (AgentFamily::Claude, "claude:home".into(), "left".into()),
            (AgentFamily::Claude, "claude:other".into(), "current".into()),
            (AgentFamily::Codex, "claude:home".into(), "current".into()),
            (AgentFamily::Claude, "claude:home".into(), "missing".into()),
        ];
        let found = store.project_conversation_owners(&keys).await.unwrap();
        assert_eq!(found.len(), 2);
        for owner in found {
            assert_eq!(owner.conversation.conversation_id, "c1");
            assert_eq!(owner.family, AgentFamily::Claude);
            assert_eq!(owner.provider_store, "claude:home");
            assert_eq!(owner.is_current, owner.native_session_id == "current");
        }
    }

    #[tokio::test]
    async fn native_reset_is_atomic_idempotent_and_preserves_queued_intent() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let old = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let new = "11111111-2222-3333-4444-555555555555";
        thread(&store, AgentFamily::Claude, Some(old)).await;
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
        let MessageInsert::Inserted(delivered) = store
            .insert_dispatch_message("owner", "old", "/clear", "hash-old", "c1", &[], "2", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        store
            .update_message_delivery(
                &delivered.id,
                "uncertain",
                Some("claude-bridge"),
                Some("old-turn"),
            )
            .await
            .unwrap();
        let MessageInsert::Inserted(queued) = store
            .insert_dispatch_message("owner", "queued", "next", "hash-next", "c1", &[], "3", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        let MessageInsert::Inserted(guide) = store
            .insert_dispatch_message(
                "owner",
                "guide",
                "Guide after clear",
                "hash-guide",
                "c1",
                &[],
                "3",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        store
            .update_message_delivery(
                &guide.id,
                "accepted_by_codex",
                Some("claude-bridge"),
                Some("guide-turn"),
            )
            .await
            .unwrap();
        let before = store
            .conversation_history_page("epoch", "c1", None, 1)
            .await
            .unwrap()
            .unwrap();
        let old_cursor = before.next_cursor.unwrap();
        store
            .save_async_question("c1", "claude-bridge", "old-turn", "question", "[]", 10000)
            .await
            .unwrap();
        sqlx::query("INSERT INTO approvals(approval_id,server_request_id,method,params_json,state,created_at,thread_id,turn_id) VALUES('old-approval','request','item/tool/requestUserInput',?, 'pending','1','claude-bridge','old-turn')")
            .bind(serde_json::json!({"_wonderRuntimeId":"runtime"}).to_string()).execute(&store.pool).await.unwrap();
        store
            .claim_history_refresh("c1", "old-reader", 100)
            .await
            .unwrap();
        assert!(store
            .reset_project_native_session("claude-bridge", new, old, &[], "4")
            .await
            .is_err());
        assert_eq!(
            store
                .reset_project_native_session(
                    "claude-bridge",
                    old,
                    new,
                    &["guide-turn".into()],
                    "4"
                )
                .await
                .unwrap(),
            ("c1".into(), 1)
        );
        let sequence = store.committed_sequence("epoch").await.unwrap();
        assert_eq!(
            store
                .reset_project_native_session("claude-bridge", old, new, &[], "5")
                .await
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            store.committed_sequence("epoch").await.unwrap(),
            sequence,
            "lost ACK replay has no second reset"
        );
        assert!(!store
            .history_refresh_is_current("c1", "old-reader")
            .await
            .unwrap());
        assert_eq!(
            store.async_questions("c1", 100).await.unwrap()[0].state,
            "expired"
        );
        assert!(store
            .begin_approval_resolution("old-approval")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .approval_turn_ended("runtime", "claude-bridge", "old-turn")
            .await
            .unwrap());
        assert!(store
            .conversation_history_page("epoch", "c1", Some(&old_cursor), 100)
            .await
            .is_err());
        let after = store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.history_revision, 1);
        assert_eq!(
            store
                .message_by_id(&delivered.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "interrupted",
            "native reset settles an old lost terminal receipt"
        );
        assert_eq!(
            after
                .messages
                .iter()
                .map(|m| m.id.clone())
                .collect::<std::collections::HashSet<_>>(),
            [queued.id.clone(), guide.id.clone()].into_iter().collect()
        );
        assert!(!store
            .runtime_turn_hidden_by_reset("claude-bridge", "guide-turn")
            .await
            .unwrap());
        assert!(store
            .runtime_turn_hidden_by_reset("claude-bridge", "old-turn")
            .await
            .unwrap());
        assert_eq!(after.codex_thread_id.as_deref(), Some("claude-bridge"));
        assert_eq!(
            store
                .runtime_binding("c1")
                .await
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some(new)
        );
        assert_eq!(
            store
                .project_conversation("c1")
                .await
                .unwrap()
                .unwrap()
                .native_session_id
                .as_deref(),
            Some(new)
        );
        assert!(
            matches!(
                store
                    .insert_dispatch_message(
                        "owner",
                        "old",
                        "/clear",
                        "hash-old",
                        "c1",
                        &[],
                        "6",
                        true
                    )
                    .await
                    .unwrap(),
                MessageInsert::Existing(_)
            ),
            "delivery receipt survives reset"
        );
        // Delayed old-turn events use the same canonical index and stay hidden.
        let payload =
            serde_json::json!({"threadId":"claude-bridge","turnId":"old-turn"}).to_string();
        sqlx::query("INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json) VALUES('c1','event','late',7,?)").bind(payload).execute(&store.pool).await.unwrap();
        let hidden: i64 = sqlx::query_scalar(
            "SELECT hidden_by_reset FROM history_entries WHERE source_id='late'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(hidden, 1);
        store
            .upsert_assistant_delta(
                "c1",
                "claude-bridge",
                "old-turn",
                "late-assistant",
                "late old reply",
                "late-delta",
                "8",
            )
            .await
            .unwrap();
        assert!(store
            .committed_conversation_snapshot("epoch", "c1")
            .await
            .unwrap()
            .unwrap()
            .assistant_messages
            .is_empty());
        assert!(store.pending_handoff("c1").await.unwrap().is_none());
        // Stale/malformed/foreign attempts cannot replace the confirmed binding.
        assert!(store
            .reset_project_native_session("claude-bridge", old, "not-a-uuid", &[], "8")
            .await
            .is_err());
        assert!(store
            .reset_project_native_session("other", old, new, &[], "8")
            .await
            .is_err());
        assert!(store
            .reset_project_native_session(
                "claude-bridge",
                "99999999-2222-3333-4444-555555555555",
                new,
                &[],
                "8"
            )
            .await
            .is_err());
    }

    async fn bind_codex(store: &Store) {
        store
            .bind_project_runtime(
                "c1",
                AgentFamily::Codex,
                "codex:home",
                "codex-thread",
                None,
                "now",
            )
            .await
            .unwrap();
    }

    fn to_claude<'a>() -> ProjectRuntimeSwitch<'a> {
        ProjectRuntimeSwitch {
            conversation_id: "c1",
            family: AgentFamily::Claude,
            provider_store: "claude:home",
            model: "claude:sonnet",
            effort: Some("medium"),
            service_tier: None,
            last_position: Some(12),
            now: "later",
            except_message: None,
            resume: None,
        }
    }

    fn to_codex<'a>(now: &'a str, resume: Option<i64>) -> ProjectRuntimeSwitch<'a> {
        ProjectRuntimeSwitch {
            family: AgentFamily::Codex,
            provider_store: "codex:home",
            model: "gpt-5",
            now,
            resume,
            ..to_claude()
        }
    }

    // Contract: ordinary writes still cannot change a thread's provider or
    // native session; only the explicit switch can.
    #[tokio::test]
    async fn only_the_switch_operation_may_cross_families() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        for sql in [
            "UPDATE project_conversations SET agent_family='claude' WHERE conversation_id='c1'",
            "UPDATE project_conversations SET provider_store='claude:home' WHERE conversation_id='c1'",
            "UPDATE project_conversations SET native_session_id='other' WHERE conversation_id='c1'",
            "UPDATE project_conversations SET model='claude:sonnet' WHERE conversation_id='c1'",
            "UPDATE runtime_bindings SET agent_family='claude' WHERE conversation_id='c1'",
        ] {
            assert!(sqlx::query(sql).execute(&store.pool).await.is_err(), "{sql}");
        }
        let edit = ProjectConversationPatch {
            model: Some("claude:sonnet"),
            ..Default::default()
        };
        assert!(store
            .update_project_conversation("c1", edit, "later")
            .await
            .is_err());
        let moved = store
            .switch_project_runtime(to_claude())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(moved.family, AgentFamily::Claude);
        // The guard row does not outlive the operation.
        let guards: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_runtime_switch_guard")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(guards, 0);
        assert!(sqlx::query(
            "UPDATE project_conversations SET agent_family='codex' WHERE conversation_id='c1'"
        )
        .execute(&store.pool)
        .await
        .is_err());
    }

    #[tokio::test]
    async fn a_switch_keeps_the_thread_and_starts_a_fresh_session() {
        for (family, model, effort, provider_store, access, approval) in [
            (
                AgentFamily::Claude,
                "claude:sonnet",
                Some("medium"),
                "claude:home",
                "full_access",
                "auto",
            ),
            (
                AgentFamily::OpenCode,
                "opencode:local/fixture",
                None,
                "opencode:home",
                "workspace",
                "ask",
            ),
        ] {
            let store = Store::connect("sqlite::memory:").await.unwrap();
            thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
            bind_codex(&store).await;
            let moved = store
                .switch_project_runtime(ProjectRuntimeSwitch {
                    family,
                    model,
                    effort,
                    provider_store,
                    ..to_claude()
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    moved.family,
                    moved.model.as_deref(),
                    moved.effort.as_deref()
                ),
                (family, Some(model), effort)
            );
            // Identity, folder and Plan stay; unsupported access does not carry
            // into OpenCode, whose own configuration controls permissions.
            assert_eq!(
                (
                    moved.title.as_str(),
                    moved.cwd.as_str(),
                    moved.project_id.as_str(),
                    moved.access_mode.as_str(),
                    moved.plan_mode,
                    moved.claude_approval.as_str(),
                    moved.provider_store.as_str(),
                ),
                (
                    "Fix",
                    "/work/app",
                    "p1",
                    access,
                    true,
                    approval,
                    provider_store
                )
            );
            assert_eq!(moved.native_session_id, None);
            assert!(store.runtime_binding("c1").await.unwrap().is_none());
            assert!(store
                .conversation_thread("c1")
                .await
                .unwrap()
                .unwrap_or_default()
                .is_empty());
            let history = store.project_runtime_history("c1").await.unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(
                (
                    history[0].family,
                    history[0].native_session_id.as_deref(),
                    history[0].runtime_thread_id.as_deref(),
                    history[0].model.as_deref(),
                    history[0].last_position,
                    history[0].switched_at.as_str()
                ),
                (
                    AgentFamily::Codex,
                    Some("codex-thread"),
                    Some("codex-thread"),
                    Some("gpt-5"),
                    Some(12),
                    "later"
                )
            );
            assert_eq!(
                store.pending_handoff("c1").await.unwrap(),
                Some(PendingHandoff {
                    history_id: history[0].id,
                    delta_after_history_id: None
                })
            );
            store
                .bind_project_runtime(
                    "c1",
                    family,
                    provider_store,
                    &format!("{}-session", family.as_str()),
                    Some("session"),
                    "later",
                )
                .await
                .unwrap();
            store.clear_pending_handoff("c1").await.unwrap();
            store.clear_pending_handoff("c1").await.unwrap();
            assert!(store.pending_handoff("c1").await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn switching_back_resumes_the_old_session_and_asks_for_a_delta_only() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        store
            .switch_project_runtime(to_claude())
            .await
            .unwrap()
            .unwrap();
        store
            .bind_project_runtime(
                "c1",
                AgentFamily::Claude,
                "claude:home",
                "claude-one",
                Some("one"),
                "t1",
            )
            .await
            .unwrap();
        store
            .set_project_native_session("c1", "one", "t1")
            .await
            .unwrap();
        store.clear_pending_handoff("c1").await.unwrap();
        let codex_entry = store.project_runtime_history("c1").await.unwrap()[0].id;
        let back = store
            .switch_project_runtime(to_codex("t2", Some(codex_entry)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.family, AgentFamily::Codex);
        assert_eq!(back.native_session_id.as_deref(), Some("codex-thread"));
        let binding = store.runtime_binding("c1").await.unwrap().unwrap();
        assert_eq!(binding.thread_id, "codex-thread");
        assert_eq!(binding.family, AgentFamily::Codex);
        // The resumed session is no longer "left"; the Claude one now is.
        let history = store.project_runtime_history("c1").await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].family, AgentFamily::Claude);
        assert_eq!(history[0].native_session_id.as_deref(), Some("one"));
        assert_eq!(
            store.pending_handoff("c1").await.unwrap(),
            Some(PendingHandoff {
                history_id: history[0].id,
                delta_after_history_id: Some(codex_entry)
            })
        );
    }

    #[tokio::test]
    async fn a_resume_that_cannot_happen_changes_nothing() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        // Unknown entry, and an entry of the wrong provider.
        for resume in [99, 1] {
            let mut request = to_claude();
            request.resume = Some(resume);
            assert!(matches!(
                store.switch_project_runtime(request).await,
                Err(sqlx::Error::Protocol(message)) if message == "resume_unavailable"
            ));
        }
        let unchanged = store.project_conversation("c1").await.unwrap().unwrap();
        assert_eq!(unchanged.family, AgentFamily::Codex);
        assert!(store.runtime_binding("c1").await.unwrap().is_some());
        assert!(store
            .project_runtime_history("c1")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_fresh_thread_needs_no_handoff_and_the_same_provider_replaces_its_session() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, None).await;
        // Nothing ran yet, so there is nothing to hand over or remember.
        store
            .switch_project_runtime(to_claude())
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .project_runtime_history("c1")
            .await
            .unwrap()
            .is_empty());
        assert!(store.pending_handoff("c1").await.unwrap().is_none());
        store
            .bind_project_runtime(
                "c1",
                AgentFamily::Claude,
                "claude:home",
                "claude-one",
                Some("one"),
                "t1",
            )
            .await
            .unwrap();
        store
            .set_project_native_session("c1", "one", "t1")
            .await
            .unwrap();
        // Replacing a session whose context delivery is uncertain.
        let replace = ProjectRuntimeSwitch {
            now: "t2",
            ..to_claude()
        };
        let replaced = store
            .switch_project_runtime(replace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replaced.family, AgentFamily::Claude);
        assert_eq!(replaced.native_session_id, None);
        let history = store.project_runtime_history("c1").await.unwrap();
        assert_eq!(history.len(), 1);
        assert!(store.pending_handoff("c1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delivery_state_is_pending_then_injected_and_can_be_forgotten() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        assert_eq!(store.handoff_delivery("c1", "t").await.unwrap(), None);
        store
            .set_handoff_delivery("c1", "t", Some(HandoffDelivery::Pending), "n")
            .await
            .unwrap();
        assert_eq!(
            store.handoff_delivery("c1", "t").await.unwrap(),
            Some(HandoffDelivery::Pending)
        );
        store
            .set_handoff_delivery("c1", "t", Some(HandoffDelivery::Injected), "n")
            .await
            .unwrap();
        assert_eq!(
            store.handoff_delivery("c1", "t").await.unwrap(),
            Some(HandoffDelivery::Injected)
        );
        store
            .set_handoff_delivery("c1", "t", None, "n")
            .await
            .unwrap();
        assert_eq!(store.handoff_delivery("c1", "t").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_message_carries_its_own_model_and_provider() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        store
            .upsert_owner_device("owner", "Phone", "{}", "now")
            .await
            .unwrap();
        // Without a staged target the message takes the thread's settings.
        let MessageInsert::Inserted(plain) = store
            .insert_dispatch_message("owner", "m1", "hi", "h1", "c1", &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        let target = store
            .project_message_target(&plain.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.family, Some(AgentFamily::Codex));
        assert_eq!(target.model.as_deref(), Some("gpt-5"));
        // A staged target is consumed by exactly the message that follows.
        store
            .stage_project_message_target(
                "owner",
                "m2",
                AgentFamily::Claude,
                "claude:sonnet",
                Some("low"),
                None,
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let MessageInsert::Inserted(next) = store
            .insert_dispatch_message("owner", "m2", "again", "h2", "c1", &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        let target = store
            .project_message_target(&next.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.family, Some(AgentFamily::Claude));
        assert_eq!(
            (target.model.as_deref(), target.effort.as_deref()),
            (Some("claude:sonnet"), Some("low"))
        );
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_message_targets")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        // The thread itself did not change: only the switch operation does that.
        let thread = store.project_conversation("c1").await.unwrap().unwrap();
        assert_eq!(thread.family, AgentFamily::Codex);
    }

    #[tokio::test]
    async fn a_switch_is_refused_for_delivered_work_and_wrong_models_but_not_queued_work() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        let wrong_model = ProjectRuntimeSwitch {
            model: "gpt-5",
            ..to_claude()
        };
        assert!(store.switch_project_runtime(wrong_model).await.is_err());
        store
            .upsert_owner_device("owner", "Phone", "{}", "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(queued) = store
            .insert_dispatch_message("owner", "client", "work", "hash", "c1", &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        // Queued work waits behind the switch; it does not block it.
        store
            .update_message_delivery(&queued.id, "streaming", Some("codex-thread"), Some("t"))
            .await
            .unwrap();
        assert!(matches!(
            store.switch_project_runtime(to_claude()).await,
            Err(sqlx::Error::Protocol(message)) if message == "provider_switch_busy"
        ));
        // The message that asked for the switch is not "other work".
        let mut allowed = to_claude();
        allowed.except_message = Some(&queued.id);
        assert!(store
            .switch_project_runtime(allowed)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .switch_project_runtime(ProjectRuntimeSwitch {
                conversation_id: "missing",
                ..to_codex("later", None)
            })
            .await
            .unwrap()
            .is_none());
    }

    /// SM-18: every surface that asks "is a turn active?" answers from the
    /// one `DeliveryState` definition, and a final message cannot go back to work.
    #[tokio::test]
    async fn every_surface_agrees_on_what_an_active_turn_is() {
        for next in DeliveryState::ALL {
            let store = Store::connect("sqlite::memory:").await.unwrap();
            thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
            bind_codex(&store).await;
            store
                .upsert_owner_device("owner", "Phone", "{}", "now")
                .await
                .unwrap();
            let MessageInsert::Inserted(message) = store
                .insert_dispatch_message("owner", "client", "work", "hash", "c1", &[], "now", true)
                .await
                .unwrap()
            else {
                panic!("inserted")
            };
            if next != DeliveryState::AcceptedByWonder {
                store
                    .update_message_delivery(
                        &message.id,
                        next.as_str(),
                        Some("codex-thread"),
                        Some("turn"),
                    )
                    .await
                    .unwrap();
            }
            let stored = store
                .message_by_device_and_client_message_id("owner", "client")
                .await
                .unwrap()
                .unwrap();
            if !DeliveryState::AcceptedByWonder.may_become(next) {
                // Not a state a queued message can enter: refused, still queued.
                assert_eq!(stored.state, "accepted_by_wonder", "{next:?}");
                continue;
            }
            assert_eq!(stored.state, next.as_str());
            let working = next.is_active(TurnScope::Admitted) || next == DeliveryState::Uncertain;
            assert_eq!(
                store.conversation_has_active_turn("c1").await.unwrap(),
                next.is_active(TurnScope::Admitted),
                "{next:?}"
            );
            assert_eq!(
                store
                    .running_conversations()
                    .await
                    .unwrap()
                    .contains(&"c1".to_string()),
                working,
                "{next:?}"
            );
            assert_eq!(
                store
                    .owned_activity(0)
                    .await
                    .unwrap()
                    .running
                    .contains(&"c1".to_string()),
                working,
                "{next:?}"
            );
            assert_eq!(
                store
                    .conversation_has_archive_blocking_work("c1")
                    .await
                    .unwrap(),
                next.is_active(TurnScope::Held),
                "{next:?}"
            );
            assert_eq!(
                store.conversation_unsettled_turns("c1").await.unwrap(),
                if next.is_active(TurnScope::Settleable) {
                    vec!["turn".to_string()]
                } else {
                    vec![]
                },
                "{next:?}"
            );
            // A reconciling uncertain receipt reads as working in the sidebar
            // exactly when a provider switch is refused for it.
            let switched = store.switch_project_runtime(to_claude()).await;
            assert_eq!(
                matches!(&switched, Err(sqlx::Error::Protocol(m)) if m == "provider_switch_busy"),
                next.is_active(TurnScope::Unsettled),
                "{next:?}"
            );
            // Final messages never return to work; a late event is ignored.
            if matches!(next, DeliveryState::Completed | DeliveryState::Interrupted) {
                store
                    .update_message_delivery(&message.id, "streaming", None, None)
                    .await
                    .unwrap();
                let after = store
                    .message_by_device_and_client_message_id("owner", "client")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(after.state, next.as_str());
                assert!(!store.conversation_has_active_turn("c1").await.unwrap());
            }
        }
    }

    /// SM-18: an unconfirmed delivery that never got a turn id (the send may
    /// or may not have reached the provider) reads as working in the sidebar
    /// and is refused by a provider switch, the same as one with a turn.
    #[tokio::test]
    async fn uncertain_delivery_without_a_turn_is_working_and_busy() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        thread(&store, AgentFamily::Codex, Some("codex-thread")).await;
        bind_codex(&store).await;
        store
            .upsert_owner_device("owner", "Phone", "{}", "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message("owner", "client", "work", "hash", "c1", &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("inserted")
        };
        store
            .fail_message_delivery(&message.id, "uncertain", "send not confirmed")
            .await
            .unwrap();
        let stored = store
            .message_by_device_and_client_message_id("owner", "client")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, "uncertain");
        assert!(stored.codex_turn_id.is_none());
        let sidebar = store.running_conversations().await.unwrap();
        assert!(sidebar.contains(&"c1".to_string()), "{sidebar:?}");
        assert!(store
            .owned_activity(0)
            .await
            .unwrap()
            .running
            .contains(&"c1".to_string()));
        let switched = store.switch_project_runtime(to_claude()).await;
        assert!(
            matches!(&switched, Err(sqlx::Error::Protocol(m)) if m == "provider_switch_busy"),
            "{switched:?}"
        );
    }
}
