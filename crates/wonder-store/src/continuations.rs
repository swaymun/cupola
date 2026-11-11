//! Work that continues without the owner: a thread stopped by a usage limit
//! resumes once the limit resets, and a reply cut off by a crash or restart
//! continues (the daemon's `update_handoff` path). Both are host-wide choices.
use super::*;

/// The host-wide continuation choices; both default on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationSettings {
    pub resume_after_limit: bool,
    pub continue_after_restart: bool,
}

/// Archiving ends a thread's scheduled resume in the archive's own
/// transaction, so restoring it later does not bring the resume back, and
/// counts the archive so a resume still being scheduled is not either.
pub(crate) async fn drop_usage_resume(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM usage_resumes WHERE conversation_id=?")
        .bind(conversation)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO thread_archive_counts(conversation_id,archive_count) VALUES(?,1) ON CONFLICT(conversation_id) DO UPDATE SET archive_count=archive_count+1")
    .bind(conversation)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    pub async fn continuation_settings(&self) -> Result<ContinuationSettings, sqlx::Error> {
        let row: Option<(bool, bool)> = sqlx::query_as(
            "SELECT resume_after_limit, continue_after_restart FROM project_settings WHERE id=1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let (resume_after_limit, continue_after_restart) = row.unwrap_or((true, true));
        Ok(ContinuationSettings {
            resume_after_limit,
            continue_after_restart,
        })
    }

    /// Saves either choice; `None` keeps it. Turning resuming off cancels
    /// every scheduled resume.
    pub async fn set_continuation_settings(
        &self,
        resume_after_limit: Option<bool>,
        continue_after_restart: Option<bool>,
    ) -> Result<ContinuationSettings, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT OR IGNORE INTO project_settings(id, include_worktrees) VALUES (1, 1)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE project_settings SET resume_after_limit=COALESCE(?,resume_after_limit), continue_after_restart=COALESCE(?,continue_after_restart) WHERE id=1")
            .bind(resume_after_limit).bind(continue_after_restart).execute(&mut *tx).await?;
        if resume_after_limit == Some(false) {
            sqlx::query("DELETE FROM usage_resumes")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.continuation_settings().await
    }

    /// How many continuations with `request_id` this message already had, so
    /// a reply that keeps crashing its runtime is not continued forever.
    pub async fn message_continuations(
        &self,
        message: &str,
        request_id: &str,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM update_handoffs WHERE message_id=? AND request_id=?",
        )
        .bind(message)
        .bind(request_id)
        .fetch_one(&self.pool)
        .await
    }

    /// How many times the Project thread was archived; `None` when it is
    /// not a Project thread or is archived now. Read before looking up a
    /// reset and passed to `schedule_usage_resume`.
    pub async fn usage_resume_archive_count(
        &self,
        conversation: &str,
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar("SELECT COALESCE((SELECT archive_count FROM thread_archive_counts WHERE conversation_id=p.conversation_id), 0) FROM project_conversations p WHERE p.conversation_id=? AND p.wonder_archived_at IS NULL")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await
    }

    /// Schedules (or moves) the conversation's resume after the usage limit
    /// that stopped `turn`, which ran `message`. In one transaction with the
    /// setting, sends and archives, so turning resuming off, the owner
    /// sending into the thread, or archiving it (`archive_count` moved past
    /// the value read before the lookup, even if restored since) while the
    /// reset was being looked up wins: nothing is scheduled then, and the
    /// result is false.
    pub async fn schedule_usage_resume(
        &self,
        conversation: &str,
        message: &str,
        turn: &str,
        archive_count: i64,
        resume_at: i64,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let resume_after_limit: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT resume_after_limit FROM project_settings WHERE id=1), 1)",
        )
        .fetch_one(&mut *tx)
        .await?;
        let sent_since: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND rowid > COALESCE((SELECT rowid FROM messages WHERE id=?), -1))")
            .bind(conversation).bind(message).fetch_one(&mut *tx).await?;
        let archived_since: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM project_conversations p WHERE p.conversation_id=? AND p.wonder_archived_at IS NULL AND COALESCE((SELECT archive_count FROM thread_archive_counts WHERE conversation_id=p.conversation_id), 0)=?)")
            .bind(conversation).bind(archive_count).fetch_one(&mut *tx).await?;
        if !resume_after_limit || sent_since || archived_since {
            return Ok(false);
        }
        sqlx::query("INSERT INTO usage_resumes(conversation_id,stopped_turn_id,stopped_message_id,resume_at,created_at) VALUES(?,?,?,?,?) ON CONFLICT(conversation_id) DO UPDATE SET stopped_turn_id=excluded.stopped_turn_id, stopped_message_id=excluded.stopped_message_id, resume_at=excluded.resume_at, created_at=excluded.created_at")
            .bind(conversation).bind(turn).bind(message).bind(resume_at).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Cancels the conversation's scheduled resume; true when one existed.
    pub async fn cancel_usage_resume(&self, conversation: &str) -> Result<bool, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM usage_resumes WHERE conversation_id=?")
                .bind(conversation)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// Every scheduled resume: conversation ID to Unix seconds.
    pub async fn usage_resumes(&self) -> Result<HashMap<String, i64>, sqlx::Error> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT conversation_id, resume_at FROM usage_resumes")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// Resumes due at `now` (Unix seconds), earliest first.
    pub async fn due_usage_resumes(&self, now: i64) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT conversation_id FROM usage_resumes WHERE resume_at<=? ORDER BY resume_at, conversation_id LIMIT 25",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
    }

    /// Turns a due resume into an owner message queued behind any other
    /// waiting message, in one transaction: the resume is consumed only if
    /// the message and its dispatch work commit. `None` when the resume was
    /// cancelled or moved meanwhile, or the thread is no longer a Project
    /// thread Cupola runs.
    pub async fn materialize_usage_resume(
        &self,
        conversation: &str,
        device_id: &str,
        body: &str,
        now_seconds: i64,
        now: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        use sha2::{Digest, Sha256};
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let stopped: Option<(String, String)> = sqlx::query_as(
            "DELETE FROM usage_resumes WHERE conversation_id=? AND resume_at<=? RETURNING stopped_turn_id, stopped_message_id",
        )
        .bind(conversation)
        .bind(now_seconds)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((turn, message)) = stopped else {
            return Ok(None);
        };
        // Resuming was turned off after this was scheduled, or the owner
        // sent into the thread since the stop (their send commits before it
        // cancels the resume, so this transaction may see it first): drop it.
        let resume_after_limit: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT resume_after_limit FROM project_settings WHERE id=1), 1)",
        )
        .fetch_one(&mut *tx)
        .await?;
        let sent_since: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND rowid > COALESCE((SELECT rowid FROM messages WHERE id=?), -1))")
            .bind(conversation).bind(&message).fetch_one(&mut *tx).await?;
        if !resume_after_limit || sent_since {
            tx.commit().await?;
            return Ok(None);
        }
        let id = format!("usage-resume:{conversation}:{turn}");
        let queue_position: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(m.queue_position), -1) + 1 FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder'")
            .bind(conversation).fetch_one(&mut *tx).await?;
        let inserted = sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at,queue_position) VALUES (?,?,?,?,?,?,'accepted_by_wonder',?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(device_id).bind(&id).bind(body).bind(hex::encode(Sha256::digest(body.as_bytes()))).bind(conversation).bind(now).bind(queue_position).execute(&mut *tx).await?;
        if inserted.rows_affected() == 0 {
            // This stop already resumed once; commit the consumed row only.
            tx.commit().await?;
            return Ok(None);
        }
        let saved = sqlx::query("INSERT OR IGNORE INTO message_execution_settings(message_id,model,reasoning_effort,service_tier,working_directory) SELECT ?,p.model,p.effort,p.service_tier,p.cwd FROM project_conversations p WHERE p.conversation_id=? AND p.wonder_archived_at IS NULL")
            .bind(&id).bind(conversation).execute(&mut *tx).await?;
        if saved.rows_affected() == 0 {
            // Not a Project thread (any more), or archived: drop the resume.
            tx.rollback().await?;
            self.cancel_usage_resume(conversation).await?;
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO dispatch_work(message_id) VALUES (?) ON CONFLICT(message_id) DO NOTHING",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.message_by_id(&id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Project thread `c`.
    async fn project_thread(store: &Store) {
        for sql in [
            "INSERT INTO conversations(id,codex_thread_id,session_id,created_at) VALUES('c',NULL,NULL,'now')",
            "INSERT INTO projects(id,name,primary_root_id,created_at,updated_at) VALUES('p','Project','root','now','now')",
            "INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,cwd,roots_revision,title,created_at,updated_at,last_activity_at) VALUES('c','p','codex','codex:home','/work',1,'Thread','now','now','now')",
        ] {
            sqlx::query(sql).execute(&store.pool).await.unwrap();
        }
    }

    #[tokio::test]
    async fn continuation_settings_default_on_and_turning_resume_off_cancels_resumes() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let on = ContinuationSettings {
            resume_after_limit: true,
            continue_after_restart: true,
        };
        assert_eq!(store.continuation_settings().await.unwrap(), on);
        project_thread(&store).await;
        assert!(store
            .schedule_usage_resume("c", "m", "turn-1", 0, 100, "now")
            .await
            .unwrap());
        // A later stop moves the same conversation's resume.
        assert!(store
            .schedule_usage_resume("c", "m", "turn-2", 0, 200, "now")
            .await
            .unwrap());
        assert_eq!(store.usage_resumes().await.unwrap()["c"], 200);
        assert!(store.due_usage_resumes(199).await.unwrap().is_empty());
        assert_eq!(store.due_usage_resumes(200).await.unwrap(), vec!["c"]);
        let restart_off = store
            .set_continuation_settings(None, Some(false))
            .await
            .unwrap();
        assert_eq!(
            (
                restart_off.resume_after_limit,
                restart_off.continue_after_restart
            ),
            (true, false)
        );
        assert_eq!(store.usage_resumes().await.unwrap().len(), 1);
        store
            .set_continuation_settings(Some(false), None)
            .await
            .unwrap();
        assert!(store.usage_resumes().await.unwrap().is_empty());
        assert!(!store.cancel_usage_resume("c").await.unwrap());
        // A cancelled or missing resume materializes nothing.
        assert!(store
            .materialize_usage_resume("c", "device", "Continue", 300, "now")
            .await
            .unwrap()
            .is_none());
    }

    /// The resume path looks the reset up (awaited, up to seconds) between
    /// noticing the limit and scheduling; whatever the owner did meanwhile wins.
    #[tokio::test]
    async fn a_send_or_turning_resuming_off_during_the_reset_lookup_is_not_undone() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES('owner','Paired phone','owner','{}','now')")
            .execute(&store.pool).await.unwrap();
        project_thread(&store).await;
        let send = |id: &'static str| {
            sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) VALUES(?,'owner',?,'work','hash','c','accepted_by_wonder','now')")
                .bind(id).bind(id).execute(&store.pool)
        };
        send("limited").await.unwrap();
        // The owner sends into the thread before the schedule lands.
        send("owner-reply").await.unwrap();
        assert!(!store
            .schedule_usage_resume("c", "limited", "turn-1", 0, 100, "now")
            .await
            .unwrap());
        assert!(store.usage_resumes().await.unwrap().is_empty());
        // A limit on the owner's own later message still schedules.
        assert!(store
            .schedule_usage_resume("c", "owner-reply", "turn-2", 0, 100, "now")
            .await
            .unwrap());
        // Resuming is turned off before the schedule lands.
        store
            .set_continuation_settings(Some(false), None)
            .await
            .unwrap();
        assert!(!store
            .schedule_usage_resume("c", "owner-reply", "turn-3", 0, 100, "now")
            .await
            .unwrap());
        assert!(store.usage_resumes().await.unwrap().is_empty());
        // A resume already due when resuming is off sends nothing either.
        store
            .set_continuation_settings(Some(true), None)
            .await
            .unwrap();
        assert!(store
            .schedule_usage_resume("c", "owner-reply", "turn-3", 0, 100, "now")
            .await
            .unwrap());
        sqlx::query("UPDATE project_settings SET resume_after_limit=0 WHERE id=1")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .materialize_usage_resume("c", "owner", "Continue", 300, "now")
            .await
            .unwrap()
            .is_none());
        assert!(store.usage_resumes().await.unwrap().is_empty());
    }

    /// Archiving while the reset is looked up wins, even if the thread was
    /// restored before the schedule lands.
    #[tokio::test]
    async fn archiving_during_the_reset_lookup_is_not_undone() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project_thread(&store).await;
        let before = store
            .usage_resume_archive_count("c")
            .await
            .unwrap()
            .unwrap();
        store
            .set_project_conversation_wonder_archived("c", true, "now")
            .await
            .unwrap();
        assert_eq!(store.usage_resume_archive_count("c").await.unwrap(), None);
        assert!(!store
            .schedule_usage_resume("c", "m", "turn-1", before, 100, "now")
            .await
            .unwrap());
        store
            .set_project_conversation_wonder_archived("c", false, "now")
            .await
            .unwrap();
        assert!(!store
            .schedule_usage_resume("c", "m", "turn-1", before, 100, "now")
            .await
            .unwrap());
        assert!(store.usage_resumes().await.unwrap().is_empty());
        // A limit after the restore schedules as usual.
        let after = store
            .usage_resume_archive_count("c")
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .schedule_usage_resume("c", "m", "turn-2", after, 100, "now")
            .await
            .unwrap());
    }
}
