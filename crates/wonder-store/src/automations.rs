use super::*;

impl Store {
    /// All editable fields and the next occurrence become visible together.
    pub async fn update_automation(&self, value: &StoredAutomation) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE automations SET name=?, kind=?, conversation_id=?, prompt=?, rrule=?, timezone=?, status=?, notification_policy=?, next_run_at=?, updated_at=?, revision=revision+1 WHERE id=? AND revision=? AND (?='paused' OR (scope_type='project_thread' AND EXISTS (SELECT 1 FROM project_conversations p JOIN projects project ON project.id=p.project_id JOIN runtime_bindings r ON r.conversation_id=p.conversation_id AND r.execution_scope='projects' AND r.agent_family=p.agent_family AND r.provider_store=p.provider_store AND r.runtime_thread_id=(SELECT codex_thread_id FROM conversations WHERE id=p.conversation_id) WHERE p.conversation_id=automations.scope_id AND project.is_included=1)))")
            .bind(&value.name).bind(&value.kind).bind(&value.conversation_id).bind(&value.prompt)
            .bind(&value.rrule).bind(&value.timezone).bind(&value.status).bind(&value.notification_policy)
            .bind(&value.next_run_at).bind(&value.updated_at).bind(&value.id).bind(value.revision).bind(&value.status)
            .execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn automation_run_by_id(
        &self,
        id: &str,
    ) -> Result<Option<StoredAutomationRun>, sqlx::Error> {
        sqlx::query("SELECT id, automation_id, scheduled_for, status, started_at, finished_at, error, message_id, conversation_id FROM automation_runs WHERE id=?")
            .bind(id).fetch_optional(&self.pool).await.map(|row|row.map(|row|stored_automation_run(&row)))
    }

    /// Atomic admission prevents overlap. The snapshot is the accepted work;
    /// schedule edits cannot rewrite it. Advancing from wall time coalesces misses.
    pub async fn claim_scheduled_automation(
        &self,
        id: &str,
        automation: &StoredAutomation,
        scheduled_for: &str,
        now: &str,
        next_run: Option<&str>,
        scheduled: bool,
    ) -> Result<bool, sqlx::Error> {
        let snapshot =
            serde_json::to_string(automation).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("INSERT OR IGNORE INTO automation_runs(id, automation_id, scheduled_for, status, started_at, automation_snapshot, conversation_id) SELECT ?, ?, ?, 'running', ?, ?, ? WHERE EXISTS (SELECT 1 FROM automations a WHERE a.id=? AND a.revision=? AND ((a.scope_type='project_thread' AND EXISTS (SELECT 1 FROM project_conversations p JOIN projects project ON project.id=p.project_id JOIN runtime_bindings r ON r.conversation_id=p.conversation_id AND r.execution_scope='projects' AND r.agent_family=p.agent_family AND r.provider_store=p.provider_store AND r.runtime_thread_id=(SELECT codex_thread_id FROM conversations WHERE id=p.conversation_id) WHERE p.conversation_id=a.scope_id AND p.conversation_id=a.conversation_id AND project.is_included=1))) AND (?=0 OR (a.status='active' AND a.next_run_at=?))) AND NOT EXISTS (SELECT 1 FROM automation_runs WHERE automation_id=? AND status='running')")
            .bind(id).bind(&automation.id).bind(scheduled_for).bind(now).bind(snapshot).bind(&automation.conversation_id)
            .bind(&automation.id).bind(automation.revision).bind(scheduled).bind(scheduled_for).bind(&automation.id)
            .execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Ok(false);
        }
        if scheduled {
            sqlx::query(
                "UPDATE automations SET next_run_at=?, last_run_at=?, updated_at=?, revision=revision+1 WHERE id=?",
            )
            .bind(next_run)
            .bind(now)
            .bind(now)
            .bind(&automation.id)
            .execute(&mut *tx)
            .await?;
        } else {
            // A manual run is still the last run; it leaves the schedule and
            // the edit revision alone so an open editor stays current.
            sqlx::query("UPDATE automations SET last_run_at=? WHERE id=?")
                .bind(now)
                .bind(&automation.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Recover unmaterialized claims; linked messages already have durable dispatch work.
    pub async fn recoverable_automation_runs(
        &self,
    ) -> Result<Vec<(StoredAutomationRun, StoredAutomation)>, sqlx::Error> {
        let rows = sqlx::query("SELECT r.id, r.automation_id, r.scheduled_for, r.status, r.started_at, r.finished_at, r.error, r.message_id, r.conversation_id, r.automation_snapshot FROM automation_runs r JOIN automations a ON a.id=r.automation_id WHERE r.status='running' AND r.automation_snapshot IS NOT NULL AND r.message_id IS NULL")
            .fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                let snapshot: String = row.get("automation_snapshot");
                serde_json::from_str(&snapshot)
                    .map(|value| (stored_automation_run(row), value))
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))
            })
            .collect()
    }
}

impl Store {
    pub async fn materialize_automation_message(
        &self,
        run_id: &str,
        device_id: &str,
        automation: &StoredAutomation,
        conversation_id: &str,
        now: &str,
    ) -> Result<StoredMessage, sqlx::Error> {
        use sha2::{Digest, Sha256};
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let id: String = sqlx::query_scalar(
            "SELECT COALESCE(message_id, ?) FROM automation_runs WHERE id=? AND status='running'",
        )
        .bind(format!("automation-message:{run_id}"))
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        match &automation.target {
            AutomationTarget::ProjectThread { scope_id }
                if scope_id == conversation_id
                    && automation.conversation_id.as_deref() == Some(scope_id) =>
            {
                let bound: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_conversations c JOIN projects p ON p.id=c.project_id JOIN runtime_bindings r ON r.conversation_id=c.conversation_id AND r.execution_scope='projects' AND r.agent_family=c.agent_family AND r.provider_store=c.provider_store AND r.runtime_thread_id=(SELECT codex_thread_id FROM conversations WHERE id=c.conversation_id) WHERE c.conversation_id=? AND p.is_included=1)")
                    .bind(scope_id).fetch_one(&mut *tx).await?;
                if bound == 0 {
                    return Err(sqlx::Error::Protocol(
                        "Project automation target is unavailable".into(),
                    ));
                }
            }
            AutomationTarget::ProjectThread { .. } => {
                return Err(sqlx::Error::Protocol(
                    "Project automation target changed".into(),
                ))
            }
        }
        let queue_position: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(m.queue_position), -1) + 1 FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder'")
            .bind(conversation_id).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at,queue_position) VALUES (?,?,?,?,?,?,'accepted_by_wonder',?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(device_id).bind(format!("automation:{run_id}")).bind(&automation.prompt).bind(hex::encode(Sha256::digest(automation.prompt.as_bytes()))).bind(conversation_id).bind(now).bind(queue_position).execute(&mut *tx).await?;
        sqlx::query("UPDATE automation_runs SET message_id=?, conversation_id=? WHERE id=?")
            .bind(&id)
            .bind(conversation_id)
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        if matches!(&automation.target, AutomationTarget::ProjectThread { .. }) {
            let saved = sqlx::query("INSERT OR IGNORE INTO message_execution_settings(message_id,model,reasoning_effort,service_tier,working_directory) SELECT ?,p.model,p.effort,p.service_tier,p.cwd FROM project_conversations p WHERE p.conversation_id=?")
                .bind(&id).bind(conversation_id).execute(&mut *tx).await?;
            if saved.rows_affected() == 0 {
                let exists: i64 = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM message_execution_settings WHERE message_id=?)",
                )
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
                if exists == 0 {
                    return Err(sqlx::Error::Protocol(
                        "Project execution settings are unavailable".into(),
                    ));
                }
            }
        }
        sqlx::query(
            "INSERT INTO dispatch_work(message_id) VALUES (?) ON CONFLICT(message_id) DO NOTHING",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.message_by_id(&id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    /// Reconcile terminal durable message states, including
    /// completions whose original notification arrived during an outage.
    pub async fn reconcile_automation_runs(&self, now: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE automation_runs SET error='Cupola is checking whether this run was received. Open its conversation for recovery; do not submit it again.' WHERE status='running' AND message_id IN (SELECT id FROM messages WHERE state='uncertain')")
            .execute(&self.pool).await?;
        sqlx::query("UPDATE automation_runs SET error=NULL WHERE status='running' AND message_id IN (SELECT id FROM messages WHERE state IN ('accepted_by_codex','streaming'))")
            .execute(&self.pool).await?;
        sqlx::query("UPDATE automation_runs SET status=CASE WHEN (SELECT state FROM messages WHERE id=automation_runs.message_id)='completed' THEN 'completed' ELSE 'failed' END, finished_at=?, error=CASE WHEN (SELECT state FROM messages WHERE id=automation_runs.message_id)='completed' THEN NULL ELSE 'The run stopped before completing. Open its conversation for details.' END WHERE status='running' AND message_id IN (SELECT id FROM messages WHERE state IN ('completed','failed','interrupted'))")
            .bind(now).execute(&self.pool).await?;
        sqlx::query("UPDATE automation_runs SET status='failed',finished_at=?,error='The run was not submitted. Open its conversation to retry safely.' WHERE status='running' AND message_id IN (SELECT id FROM messages WHERE state='safe_to_retry')")
            .bind(now).execute(&self.pool).await?;
        Ok(())
    }
}

/// The latest automation run that posted into a thread, for its list row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadAutomationRun {
    pub conversation_id: String,
    pub automation_id: String,
    /// Null if the automation record is gone.
    pub automation_name: Option<String>,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

impl Store {
    /// One read for a whole list page: the newest run per thread, so a list
    /// of hundreds of threads never asks once per row.
    pub async fn latest_automation_runs(
        &self,
        conversation_ids: &[String],
    ) -> Result<HashMap<String, ThreadAutomationRun>, sqlx::Error> {
        let mut latest = HashMap::new();
        for ids in conversation_ids.chunks(500) {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "SELECT conversation_id, automation_id, name, status, started_at, finished_at, error FROM (SELECT r.conversation_id, r.automation_id, a.name, r.status, r.started_at, r.finished_at, r.error, ROW_NUMBER() OVER (PARTITION BY r.conversation_id ORDER BY r.started_at DESC, r.id DESC) AS position FROM automation_runs r LEFT JOIN automations a ON a.id = r.automation_id WHERE r.conversation_id IN (",
            );
            let mut separated = query.separated(", ");
            for id in ids {
                separated.push_bind(id);
            }
            separated.push_unseparated(")) WHERE position = 1");
            for row in query.build().fetch_all(&self.pool).await? {
                let run = ThreadAutomationRun {
                    conversation_id: row.get("conversation_id"),
                    automation_id: row.get("automation_id"),
                    automation_name: row.get("name"),
                    status: row.get("status"),
                    started_at: row.get("started_at"),
                    finished_at: row.get("finished_at"),
                    error: row.get("error"),
                };
                latest.insert(run.conversation_id.clone(), run);
            }
        }
        Ok(latest)
    }

    /// Labels for the messages automations posted into a thread, keyed by
    /// message id, so its history shows which ones the owner did not type.
    /// Read from the message's own id, so the label outlives the automation;
    /// the title is its name while the automation still exists.
    pub async fn automation_message_sources(
        &self,
        conversation_id: &str,
    ) -> Result<HashMap<String, crate::MessageSource>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.id, a.name FROM messages m LEFT JOIN automation_runs r ON r.message_id = m.id LEFT JOIN automations a ON a.id = r.automation_id WHERE m.conversation_id = ? AND m.id LIKE 'automation-message:%'")
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let name: Option<String> = row.get("name");
                (
                    row.get("id"),
                    crate::MessageSource {
                        kind: "automation".into(),
                        source_conversation_id: None,
                        source_title: name.unwrap_or_else(|| "Automation".into()),
                    },
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentFamily, ProjectConversationInsert, ProjectRootInput};
    async fn fixture(store: &Store) -> StoredAutomation {
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        seed_project_thread(
            store,
            "automation:auto",
            "Project",
            "/work/automation",
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        store
            .set_project_native_session("automation:auto", "native-auto", "now")
            .await
            .unwrap();
        store
            .bind_project_runtime(
                "automation:auto",
                AgentFamily::Codex,
                "fixture-store",
                "native-auto",
                None,
                "now",
            )
            .await
            .unwrap();
        store
            .insert_scoped_automation(
                "auto",
                "Daily",
                "continuation",
                "project_thread",
                "automation:auto",
                Some("automation:auto"),
                "Original task",
                "FREQ=DAILY;BYHOUR=9",
                "UTC",
                "active",
                "all_runs",
                Some("2026-09-01T09:00:00.000Z"),
                "now",
            )
            .await
            .unwrap()
    }

    async fn project_fixture(store: &Store) -> StoredAutomation {
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        store
            .create_project(
                "project",
                "project-request",
                "hash",
                "Project",
                &[ProjectRootInput {
                    path: "/work/project".into(),
                    canonical_path: "/work/project".into(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "project-thread",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: "codex:home",
                native_session_id: Some("native-project-thread"),
                cwd: "/work/project",
                roots_revision: 1,
                title: "Project thread",
                model: Some("gpt-test"),
                effort: Some("high"),
                service_tier: Some("fast"),
                access_mode: "workspace",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        store
            .bind_project_runtime(
                "project-thread",
                AgentFamily::Codex,
                "codex:home",
                "native-project-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        store
            .insert_scoped_automation(
                "project-auto",
                "Project task",
                "continuation",
                "project_thread",
                "project-thread",
                Some("project-thread"),
                "Continue the work",
                "FREQ=DAILY;BYHOUR=9",
                "UTC",
                "active",
                "all_runs",
                Some("2026-09-01T09:00:00.000Z"),
                "now",
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn project_claim_materializes_one_bound_message_and_preserves_receipt() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let automation = project_fixture(&store).await;
        assert_eq!(
            automation.target,
            AutomationTarget::ProjectThread {
                scope_id: "project-thread".into()
            }
        );
        assert!(store
            .claim_scheduled_automation("project-run", &automation, "manual", "now", None, false)
            .await
            .unwrap());
        assert!(!store
            .claim_scheduled_automation("duplicate", &automation, "manual2", "now", None, false)
            .await
            .unwrap());
        let message = store
            .materialize_automation_message(
                "project-run",
                "owner",
                &automation,
                "project-thread",
                "now",
            )
            .await
            .unwrap();
        assert_eq!(message.id, "automation-message:project-run");
        assert_eq!(message.conversation_id, "project-thread");
        assert_eq!(message.client_message_id, "automation:project-run");
        let setting: (Option<String>, Option<String>, Option<String>, String) = sqlx::query_as(
            "SELECT model,reasoning_effort,service_tier,working_directory FROM message_execution_settings WHERE message_id=?")
            .bind(&message.id).fetch_one(&store.pool).await.unwrap();
        assert_eq!(
            setting,
            (
                Some("gpt-test".into()),
                Some("high".into()),
                Some("fast".into()),
                "/work/project".into()
            )
        );
        let work: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dispatch_work WHERE message_id=?")
            .bind(&message.id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(work, 1);
        assert!(store
            .recoverable_automation_runs()
            .await
            .unwrap()
            .is_empty());
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission_with_context(&message.id, "native-project-thread", None)
            .await
            .unwrap();
        store.recover_dispatch_claims().await.unwrap();
        assert!(!store.claim_message_for_dispatch(&message.id).await.unwrap());
    }

    #[tokio::test]
    async fn project_claim_survives_restart_before_and_after_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("project.sqlite").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let automation = project_fixture(&store).await;
        assert!(store
            .claim_scheduled_automation(
                "project-restart",
                &automation,
                "manual",
                "now",
                None,
                false
            )
            .await
            .unwrap());
        store.pool.close().await;

        let store = Store::connect(&url).await.unwrap();
        let pending = store.recoverable_automation_runs().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, automation);
        let message = store
            .materialize_automation_message(
                "project-restart",
                "owner",
                &pending[0].1,
                "project-thread",
                "now",
            )
            .await
            .unwrap();
        store.pool.close().await;

        let store = Store::connect(&url).await.unwrap();
        assert!(store
            .recoverable_automation_runs()
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store.message_by_id(&message.id).await.unwrap().unwrap().id,
            message.id
        );
        assert_eq!(
            store
                .automation_run_by_id("project-restart")
                .await
                .unwrap()
                .unwrap()
                .message_id
                .as_deref(),
            Some(message.id.as_str())
        );
        let messages: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE conversation_id='project-thread'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn project_claim_recovers_materialization_failure_without_restart() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let automation = project_fixture(&store).await;
        let scheduled_for = automation.next_run_at.as_deref().unwrap();
        let now = "2026-09-08T12:00:00.000Z";
        let next = "2026-09-09T09:00:00.000Z";
        assert!(store
            .claim_scheduled_automation(
                "recover-run",
                &automation,
                scheduled_for,
                now,
                Some(next),
                true,
            )
            .await
            .unwrap());
        // Fail after the message, run link and execution settings have been written.
        sqlx::query("CREATE TRIGGER fail_automation_materialization BEFORE INSERT ON dispatch_work BEGIN SELECT RAISE(ABORT, 'fixture materialization failure'); END")
            .execute(&store.pool).await.unwrap();
        let error = store
            .materialize_automation_message(
                "recover-run",
                "owner",
                &automation,
                "project-thread",
                now,
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("fixture materialization failure"));
        let run = store
            .automation_run_by_id("recover-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, "running");
        assert!(run.message_id.is_none());
        for (table, statement) in [
            ("messages", "SELECT COUNT(*) FROM messages"),
            (
                "message_execution_settings",
                "SELECT COUNT(*) FROM message_execution_settings",
            ),
            ("dispatch_work", "SELECT COUNT(*) FROM dispatch_work"),
        ] {
            let count: i64 = sqlx::query_scalar(statement)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{table} must roll back with materialization");
        }
        let mut edited = store
            .automation_by_id(&automation.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.next_run_at.as_deref(), Some(next));
        edited.prompt = "A later edit must not rewrite accepted work".into();
        assert!(store.update_automation(&edited).await.unwrap());
        let current = store
            .automation_by_id(&automation.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!store
            .claim_scheduled_automation("overlap", &current, "manual", now, None, false)
            .await
            .unwrap());
        sqlx::query("DROP TRIGGER fail_automation_materialization")
            .execute(&store.pool)
            .await
            .unwrap();
        let pending = store.recoverable_automation_runs().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].1, automation,
            "recovery uses the accepted snapshot"
        );
        let message = store
            .materialize_automation_message(
                "recover-run",
                "owner",
                &pending[0].1,
                "project-thread",
                now,
            )
            .await
            .unwrap();
        let duplicate = store
            .materialize_automation_message(
                "recover-run",
                "owner",
                &pending[0].1,
                "project-thread",
                now,
            )
            .await
            .unwrap();
        assert_eq!(message.id, duplicate.id);
        assert_eq!(message.body, automation.prompt);
        assert!(store
            .recoverable_automation_runs()
            .await
            .unwrap()
            .is_empty());
        for (table, statement) in [
            ("messages", "SELECT COUNT(*) FROM messages"),
            (
                "message_execution_settings",
                "SELECT COUNT(*) FROM message_execution_settings",
            ),
            ("dispatch_work", "SELECT COUNT(*) FROM dispatch_work"),
        ] {
            let count: i64 = sqlx::query_scalar(statement)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(count, 1, "{table} is materialized exactly once");
        }
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        assert!(!store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .finish_automation_run("recover-run", "completed", now, None, None)
            .await
            .unwrap();
        assert!(store
            .claim_scheduled_automation("next-run", &current, "manual", now, None, false)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn project_target_requires_binding_and_included_project_at_each_claim() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let automation = project_fixture(&store).await;
        sqlx::query("DELETE FROM runtime_bindings WHERE conversation_id='project-thread'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(!store
            .claim_scheduled_automation("unbound", &automation, "manual", "now", None, false)
            .await
            .unwrap());
        assert!(store
            .insert_scoped_automation(
                "bad",
                "Bad",
                "continuation",
                "project_thread",
                "project-thread",
                Some("project-thread"),
                "work",
                "FREQ=DAILY",
                "UTC",
                "active",
                "all_runs",
                None,
                "now"
            )
            .await
            .is_err());
        store
            .bind_project_runtime(
                "project-thread",
                AgentFamily::Codex,
                "codex:home",
                "native-project-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        assert!(store
            .claim_scheduled_automation(
                "claimed-before-exclusion",
                &automation,
                "manual",
                "now",
                None,
                false
            )
            .await
            .unwrap());
        sqlx::query("UPDATE projects SET is_included=0 WHERE id='project'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .materialize_automation_message(
                "claimed-before-exclusion",
                "owner",
                &automation,
                "project-thread",
                "now"
            )
            .await
            .is_err());
        assert!(!store
            .claim_scheduled_automation("excluded", &automation, "manual", "now", None, false)
            .await
            .unwrap());
    }
    #[tokio::test]
    async fn claims_are_atomic_and_coalesce_missed_occurrences() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let automation = fixture(&store).await;
        let now = "2026-09-08T12:00:00.000Z";
        let next = "2026-09-09T09:00:00.000Z";
        assert!(store
            .claim_scheduled_automation(
                "run1",
                &automation,
                automation.next_run_at.as_deref().unwrap(),
                now,
                Some(next),
                true
            )
            .await
            .unwrap());
        let current = store.automation_by_id("auto").await.unwrap().unwrap();
        assert_eq!(current.next_run_at.as_deref(), Some(next));
        assert!(store.list_due_automations(now).await.unwrap().is_empty());
        assert!(!store
            .claim_scheduled_automation("run2", &current, "manual", now, None, false)
            .await
            .unwrap());
        assert!(!store
            .claim_scheduled_automation("run1", &current, "manual", now, None, false)
            .await
            .unwrap());
        store
            .finish_automation_run("run1", "completed", now, None, None)
            .await
            .unwrap();
        assert!(store
            .claim_scheduled_automation("run2", &current, "manual", now, None, false)
            .await
            .unwrap());
        let current = store.automation_by_id("auto").await.unwrap().unwrap();
        assert_eq!(current.last_attempt_at.as_deref(), Some(now));
        assert_eq!(current.last_success_at.as_deref(), Some(now));
    }
    #[tokio::test]
    async fn edits_preserve_accepted_snapshot_and_reject_stale_schedule_claims() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let original = fixture(&store).await;
        assert!(store
            .claim_scheduled_automation(
                "run1",
                &original,
                "manual",
                "2026-09-01T01:00:00.000Z",
                None,
                false
            )
            .await
            .unwrap());
        let mut edited = original.clone();
        edited.prompt = "New task".into();
        edited.rrule = "FREQ=HOURLY".into();
        edited.timezone = "America/New_York".into();
        edited.status = "paused".into();
        edited.next_run_at = None;
        edited.updated_at = "2026-09-01T02:00:00.000Z".into();
        assert!(store.update_automation(&edited).await.unwrap());
        // A restart recovers the claimed run with the prompt it accepted.
        let recovered = store.recoverable_automation_runs().await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            (recovered[0].0.id.as_str(), recovered[0].1.prompt.as_str()),
            ("run1", "Original task")
        );
        let actual = store.automation_by_id("auto").await.unwrap().unwrap();
        assert_eq!(actual.prompt, "New task");
        assert_eq!(actual.next_run_at, None);
        assert_eq!(actual.status, "paused");
        store
            .finish_automation_run("run1", "completed", "later", None, None)
            .await
            .unwrap();
        assert!(!store
            .claim_scheduled_automation(
                "run2",
                &original,
                original.next_run_at.as_deref().unwrap(),
                "now",
                Some("next"),
                true
            )
            .await
            .unwrap());
        store
            .update_project_metadata("project-automation:auto", None, Some(false), None, "now")
            .await
            .unwrap();
        assert!(!store
            .claim_scheduled_automation("run3", &actual, "manual", "now", None, false)
            .await
            .unwrap());
    }
    #[tokio::test]
    async fn revision_rejects_stale_edit_and_scheduled_claim_even_when_timestamps_match() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let original = fixture(&store).await;
        assert_eq!(original.revision, 0);

        let mut first = original.clone();
        first.prompt = "New prompt".into();
        first.updated_at = original.updated_at.clone();
        assert!(store.update_automation(&first).await.unwrap());
        let mut stale = original.clone();
        stale.status = "paused".into();
        stale.next_run_at = None;
        assert!(!store.update_automation(&stale).await.unwrap());
        let current = store.automation_by_id(&original.id).await.unwrap().unwrap();
        assert_eq!(current.revision, 1);
        assert_eq!(current.prompt, "New prompt");
        assert_eq!(current.status, "active");

        let due = store
            .list_due_automations("2026-09-01T10:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].revision, current.revision);
        let scheduled_for = due[0].next_run_at.as_deref().unwrap();
        assert!(store
            .claim_scheduled_automation("run", &due[0], scheduled_for, "now", None, true)
            .await
            .unwrap());
        let claimed = store.automation_by_id(&original.id).await.unwrap().unwrap();
        assert_eq!(claimed.revision, 2);
        assert_eq!(claimed.next_run_at, None);
        let mut stale_after_claim = current.clone();
        stale_after_claim.prompt = "Overwrite scheduled run".into();
        assert!(!store.update_automation(&stale_after_claim).await.unwrap());
        assert_eq!(
            store
                .automation_by_id(&original.id)
                .await
                .unwrap()
                .unwrap()
                .prompt,
            "New prompt"
        );
    }

    #[tokio::test]
    async fn restart_materializes_once_and_does_not_replay_unknown_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("store.sqlite").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let automation = fixture(&store).await;
        assert!(store
            .claim_scheduled_automation(
                "run1",
                &automation,
                "manual",
                "2026-09-01T01:00:00.000Z",
                None,
                false
            )
            .await
            .unwrap());
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        let runs = store.recoverable_automation_runs().await.unwrap();
        assert_eq!(runs.len(), 1);
        let message = store
            .materialize_automation_message(
                "run1",
                "owner",
                &runs[0].1,
                "automation:auto",
                "2026-09-01T01:00:00.000Z",
            )
            .await
            .unwrap();
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission_with_context(&message.id, "thread", None)
            .await
            .unwrap();
        store.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        store.recover_dispatch_claims().await.unwrap();
        store
            .upsert_owner_device("new-owner", "Owner", "{}", "2026-09-02T00:00:00.000Z")
            .await
            .unwrap();
        let recovered = store
            .materialize_automation_message(
                "run1",
                "new-owner",
                &automation,
                "automation:auto",
                "2026-09-01T01:00:00.000Z",
            )
            .await
            .unwrap();
        assert_eq!(recovered.id, message.id);
        assert!(!store.claim_message_for_dispatch(&message.id).await.unwrap());
        assert!(store.pending_dispatch_messages().await.unwrap().is_empty());
        let run = store.automation_run_by_id("run1").await.unwrap().unwrap();
        assert_eq!(run.conversation_id.as_deref(), Some("automation:auto"));
        assert_eq!(run.status, "running");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
    #[tokio::test]
    async fn run_terminal_state_reconciles_from_durable_message() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let automation = fixture(&store).await;
        store
            .claim_scheduled_automation("run1", &automation, "manual", "now", None, false)
            .await
            .unwrap();
        let message = store
            .materialize_automation_message("run1", "owner", &automation, "automation:auto", "now")
            .await
            .unwrap();
        store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        store.complete_message_if_active(&message.id).await.unwrap();
        store
            .reconcile_automation_runs("2026-09-01T02:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(
            store
                .automation_run_by_id("run1")
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            store
                .automation_by_id("auto")
                .await
                .unwrap()
                .unwrap()
                .last_success_at
                .as_deref(),
            Some("2026-09-01T02:00:00.000Z")
        );
    }
}
