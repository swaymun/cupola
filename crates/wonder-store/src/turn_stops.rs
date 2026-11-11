//! The owner's Stop on a running turn, persisted so recovery after a host
//! restart settles the turn as stopped instead of continuing it.
use super::*;

/// A Stop the provider never confirmed is settled within seconds; a week
/// bounds the table without a separate sweep.
const STOP_RETENTION_SECS: i64 = 7 * 24 * 3600;

impl Store {
    /// Records the Stop (idempotent) and drops stale ones.
    pub async fn record_turn_stop(
        &self,
        thread: &str,
        turn: &str,
        now_secs: i64,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM turn_stop_requests WHERE requested_at < ?")
            .bind(now_secs - STOP_RETENTION_SECS)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT OR REPLACE INTO turn_stop_requests(thread_id,turn_id,requested_at) VALUES(?,?,?)")
            .bind(thread).bind(turn).bind(now_secs).execute(&mut *tx).await?;
        tx.commit().await
    }

    pub async fn turn_stop_recorded(&self, thread: &str, turn: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM turn_stop_requests WHERE thread_id=? AND turn_id=?)",
        )
        .bind(thread)
        .bind(turn)
        .fetch_one(&self.pool)
        .await
    }

    /// Once the turn is terminal the Stop has nothing left to say.
    pub async fn clear_turn_stop(&self, thread: &str, turn: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM turn_stop_requests WHERE thread_id=? AND turn_id=?")
            .bind(thread)
            .bind(turn)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn turn_stop_persists_until_cleared_and_old_ones_expire() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("w.db").display());
        let store = Store::connect(&url).await.unwrap();
        assert!(!store.turn_stop_recorded("t", "u").await.unwrap());
        store.record_turn_stop("t", "u", 1_000_000).await.unwrap();
        store.record_turn_stop("t", "u", 1_000_001).await.unwrap();
        assert!(store.turn_stop_recorded("t", "u").await.unwrap());
        assert!(!store.turn_stop_recorded("t", "other").await.unwrap());
        store
            .record_turn_stop("t", "v", 1_000_000 + STOP_RETENTION_SECS + 5)
            .await
            .unwrap();
        assert!(!store.turn_stop_recorded("t", "u").await.unwrap());
        store.clear_turn_stop("t", "v").await.unwrap();
        assert!(!store.turn_stop_recorded("t", "v").await.unwrap());
    }
}
