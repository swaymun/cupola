use crate::Store;
use sqlx::Row;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTimeLimit {
    pub conversation_id: String,
    pub thread_id: String,
    pub budget_seconds: i64,
}

impl Store {
    pub async fn goal_conversation_for_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT conversation_id FROM goal_turns WHERE thread_id=? AND turn_id=?")
            .bind(thread_id)
            .bind(turn_id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn set_goal_turn(
        &self,
        conversation_id: &str,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO goal_turns(thread_id,turn_id,conversation_id) VALUES(?,?,?)",
        )
        .bind(thread_id)
        .bind(turn_id)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn goal_conversation_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT conversation_id FROM goal_threads WHERE thread_id=?")
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn set_goal_thread(
        &self,
        conversation_id: &str,
        thread_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO goal_threads(conversation_id,thread_id) VALUES(?,?) ON CONFLICT(conversation_id) DO UPDATE SET thread_id=excluded.thread_id")
            .bind(conversation_id).bind(thread_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn clear_goal_thread(&self, thread_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM goal_threads WHERE thread_id=?")
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Goal updates kept for a thread no chat is bound to yet. Only the
    /// latest matters, so a thread that is never opened keeps one.
    pub async fn delete_pending_goal_updates(&self, thread_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM pending_app_server_notifications WHERE thread_id=? AND method IN ('thread/goal/updated','thread/goal/cleared')")
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether Cupola is submitting a message here and has no turn receipt yet.
    pub async fn conversation_submitting(
        &self,
        conversation_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let submitting: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state='dispatching_to_codex')")
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(submitting != 0)
    }

    pub async fn goal_time_limit(
        &self,
        conversation_id: &str,
    ) -> Result<Option<GoalTimeLimit>, sqlx::Error> {
        let row = sqlx::query("SELECT conversation_id,thread_id,budget_seconds FROM goal_time_limits WHERE conversation_id=?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| GoalTimeLimit {
            conversation_id: row.get("conversation_id"),
            thread_id: row.get("thread_id"),
            budget_seconds: row.get("budget_seconds"),
        }))
    }

    pub async fn set_goal_time_limit(
        &self,
        conversation_id: &str,
        thread_id: &str,
        budget_seconds: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        if let Some(seconds) = budget_seconds {
            sqlx::query("INSERT INTO goal_time_limits(conversation_id,thread_id,budget_seconds) VALUES(?,?,?) ON CONFLICT(conversation_id) DO UPDATE SET thread_id=excluded.thread_id,budget_seconds=excluded.budget_seconds")
                .bind(conversation_id)
                .bind(thread_id)
                .bind(seconds)
                .execute(&self.pool)
                .await?;
        } else {
            self.clear_goal_time_limit(conversation_id).await?;
        }
        Ok(())
    }

    pub async fn clear_goal_time_limit(&self, conversation_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM goal_time_limits WHERE conversation_id=?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn active_goal_time_limits(&self) -> Result<Vec<GoalTimeLimit>, sqlx::Error> {
        let rows =
            sqlx::query("SELECT conversation_id,thread_id,budget_seconds FROM goal_time_limits")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|row| GoalTimeLimit {
                conversation_id: row.get("conversation_id"),
                thread_id: row.get("thread_id"),
                budget_seconds: row.get("budget_seconds"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn goal_turn_mapping_survives_clear_and_time_budget_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.sqlite3").display()
        );
        let store = Store::connect(&url).await.unwrap();
        crate::bind_project_fixture(
            &store,
            "chat",
            crate::AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        store.set_goal_thread("chat", "thread").await.unwrap();
        store
            .set_goal_turn("chat", "thread", "automatic-turn")
            .await
            .unwrap();
        store
            .set_goal_time_limit("chat", "thread", Some(600))
            .await
            .unwrap();
        drop(store);

        let store = Store::connect(&url).await.unwrap();
        assert_eq!(
            store
                .goal_conversation_for_thread("thread")
                .await
                .unwrap()
                .as_deref(),
            Some("chat")
        );
        assert_eq!(
            store
                .goal_time_limit("chat")
                .await
                .unwrap()
                .unwrap()
                .budget_seconds,
            600
        );
        store.clear_goal_thread("thread").await.unwrap();
        assert_eq!(
            store.goal_conversation_for_thread("thread").await.unwrap(),
            None
        );
        assert_eq!(
            store
                .goal_conversation_for_turn("thread", "automatic-turn")
                .await
                .unwrap()
                .as_deref(),
            Some("chat")
        );
        assert_eq!(
            store
                .approval_conversation_id("thread", "automatic-turn")
                .await
                .unwrap()
                .as_deref(),
            Some("chat")
        );
    }
}
