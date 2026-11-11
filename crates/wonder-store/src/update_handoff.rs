use super::*;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct UpdateHandoff {
    pub thread_id: String,
    pub stopped_turn_id: String,
    pub request_id: String,
    pub conversation_id: String,
    pub message_id: Option<String>,
    pub resume_client_id: String,
    pub resume_params: String,
    pub turn_params: String,
    pub state: String,
    pub resumed_turn_id: Option<String>,
}

impl Store {
    pub async fn cancel_update_handoff_tree(
        &self,
        conversation: &str,
        thread: &str,
        turn: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("WITH RECURSIVE descendants(id) AS (SELECT conversation_id FROM subagent_ownership WHERE parent_conversation_id=? UNION SELECT s.conversation_id FROM subagent_ownership s JOIN descendants d ON s.parent_conversation_id=d.id) UPDATE update_handoffs SET state='cancelled' WHERE state IN ('paused','submitting') AND ((thread_id=? AND stopped_turn_id=?) OR conversation_id IN (SELECT id FROM descendants))")
            .bind(conversation).bind(thread).bind(turn).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn conversation_update_handoffs(
        &self,
        conversation: &str,
    ) -> Result<Vec<UpdateHandoff>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM update_handoffs WHERE conversation_id=? ORDER BY rowid")
            .bind(conversation)
            .fetch_all(&self.pool)
            .await
    }

    pub async fn is_update_continuation(
        &self,
        thread: &str,
        client: &str,
    ) -> Result<bool, sqlx::Error> {
        let found: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM update_handoffs WHERE thread_id=? AND resume_client_id=?)",
        )
        .bind(thread)
        .bind(client)
        .fetch_one(&self.pool)
        .await?;
        Ok(found != 0)
    }

    pub async fn note_update_handoff_error(
        &self,
        handoff: &UpdateHandoff,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE update_handoffs SET reported_error=? WHERE thread_id=? AND stopped_turn_id=? AND reported_error IS NOT ?")
            .bind(error).bind(&handoff.thread_id).bind(&handoff.stopped_turn_id).bind(error)
            .execute(&self.pool).await?.rows_affected() == 1)
    }
    pub async fn save_update_turn_settings(
        &self,
        thread: &str,
        resume: &str,
        turn: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO update_turn_settings VALUES(?,?,?) ON CONFLICT(thread_id) DO UPDATE SET resume_params=excluded.resume_params,turn_params=excluded.turn_params")
            .bind(thread).bind(resume).bind(turn).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn update_turn_settings(
        &self,
        thread: &str,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT resume_params,turn_params FROM update_turn_settings WHERE thread_id=?",
        )
        .bind(thread)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn save_update_handoff(&self, handoff: &UpdateHandoff) -> Result<(), sqlx::Error> {
        // Never revive a cancelled handoff when the updater retries preparation.
        sqlx::query("INSERT INTO update_handoffs(thread_id,stopped_turn_id,request_id,conversation_id,message_id,resume_client_id,resume_params,turn_params,state) VALUES(?,?,?,?,?,?,?,?,'paused') ON CONFLICT(thread_id,stopped_turn_id) DO NOTHING")
            .bind(&handoff.thread_id).bind(&handoff.stopped_turn_id).bind(&handoff.request_id)
            .bind(&handoff.conversation_id).bind(&handoff.message_id).bind(&handoff.resume_client_id)
            .bind(&handoff.resume_params).bind(&handoff.turn_params).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn pending_update_handoffs(&self) -> Result<Vec<UpdateHandoff>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM update_handoffs WHERE state IN ('paused','submitting') ORDER BY rowid DESC")
            .fetch_all(&self.pool).await
    }

    pub async fn update_handoff(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<UpdateHandoff>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM update_handoffs WHERE thread_id=? AND stopped_turn_id=?")
            .bind(thread)
            .bind(turn)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn cancel_update_handoff(&self, thread: &str, turn: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE update_handoffs SET state='cancelled' WHERE thread_id=? AND stopped_turn_id=? AND state IN ('paused','submitting')")
            .bind(thread).bind(turn).execute(&self.pool).await?;
        Ok(())
    }

    /// A restart continuation is claimed only while "Continue threads after
    /// restarts" is on, in the same statement, so turning it off wins.
    pub async fn claim_update_continuation(
        &self,
        handoff: &UpdateHandoff,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE update_handoffs SET state='submitting' WHERE thread_id=? AND stopped_turn_id=? AND state='paused' AND (request_id<>'restart' OR COALESCE((SELECT continue_after_restart FROM project_settings WHERE id=1),1))")
            .bind(&handoff.thread_id).bind(&handoff.stopped_turn_id).execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn finish_update_continuation(
        &self,
        handoff: &UpdateHandoff,
        new_turn: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE update_handoffs SET state='resumed',resumed_turn_id=? WHERE thread_id=? AND stopped_turn_id=? AND state='submitting'")
            .bind(new_turn).bind(&handoff.thread_id).bind(&handoff.stopped_turn_id).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            if let Some(message) = &handoff.message_id {
                sqlx::query("UPDATE messages SET codex_turn_id=?,state='accepted_by_codex' WHERE id=? AND codex_thread_id=? AND codex_turn_id=? AND state IN ('accepted_by_codex','streaming')")
                    .bind(new_turn).bind(message).bind(&handoff.thread_id).bind(&handoff.stopped_turn_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(changed == 1)
    }

    pub async fn has_unsafe_update_work(&self, host: &str) -> Result<bool, sqlx::Error> {
        // Queued work and automation runs are already durable. An
        // in-flight submit without a native receipt cannot safely be replayed.
        let unsafe_work: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE state='dispatching_to_codex' OR (state IN ('accepted_by_codex','streaming') AND (codex_thread_id IS NULL OR codex_turn_id IS NULL))) OR EXISTS(SELECT 1 FROM computer_sessions WHERE host_installation_id=? AND state IN ('preparing','awaitingSource','live','paused','stale')) OR EXISTS(SELECT 1 FROM computer_control_leases WHERE host_installation_id=? AND status='active')")
            .bind(host).bind(host).fetch_one(&self.pool).await?;
        Ok(unsafe_work != 0)
    }
}
