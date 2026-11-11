//! Global replay budgets and periodic cleanup of records that can no longer
//! be used. Canonical history, including full tool output, and pending intent
//! that may still be delivered are never pruned.
use super::*;

pub const REPLAY_MAX_EVENTS: i64 = 100_000;
pub const REPLAY_MAX_BYTES: i64 = 128 * 1024 * 1024;
const REPLAY_MAX_AGE_SECONDS: i64 = 7 * 24 * 60 * 60;
const PRUNE_BATCH: i64 = 256;
/// Notifications wait here until dispatch maps their turn. A week-old row whose
/// thread no conversation, message or goal knows will never be delivered.
pub const ORPHANED_NOTIFICATION_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// How often the maintenance loop runs `run_periodic_cleanup`; Store::connect
/// runs it once at startup.
pub const PERIODIC_CLEANUP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// A signed action is refused once its issue time is a minute away from the
/// host clock, so a nonce cannot be replayed two minutes after its claim. An
/// hour leaves room for clock corrections.
const ACTION_NONCE_RETENTION_MS: i64 = 60 * 60 * 1000;
/// Free pages are reused before the file grows, so some are kept; above
/// this, each cleanup returns at most `RECLAIM_MAX_BYTES` to the disk.
const RECLAIM_SLACK_BYTES: i64 = 16 * 1024 * 1024;
const RECLAIM_MAX_BYTES: i64 = 32 * 1024 * 1024;
const VACUUM_DISK_MARGIN_BYTES: i64 = 512 * 1024 * 1024;

#[derive(Debug)]
pub struct PeriodicCleanup {
    pub expired_sessions: u64,
    pub spent_action_nonces: u64,
    pub undeliverable_notifications: u64,
    pub reclaimed_bytes: i64,
}

#[derive(Debug)]
pub struct ReplayStorageUsage {
    pub event_count: i64,
    pub payload_bytes: i64,
    pub database_bytes: i64,
    pub freelist_bytes: i64,
}

impl Store {
    /// Each writer lock deletes at most 256 rows; release it between batches so
    /// controls can commit even after a long offline interval or an upgrade.
    pub async fn prune_replay(&self) -> Result<(), sqlx::Error> {
        while self.prune_replay_batch().await? {
            tokio::task::yield_now().await;
        }
        while self.prune_notification_receipts_batch().await? {
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    /// A receipt only dedupes a provider redelivering an event already applied
    /// (an inbox replay releases its own receipt first, and buffered rows are
    /// stored released). Past the replay age, like an orphaned buffered row, no
    /// redelivery is expected, so the receipt goes with the replay it guarded.
    /// Only wonderd's millisecond receipt times are compared.
    async fn prune_notification_receipts_batch(&self) -> Result<bool, sqlx::Error> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let cutoff = (now_ms - REPLAY_MAX_AGE_SECONDS * 1000).to_string();
        let result = sqlx::query(
            "DELETE FROM app_server_notification_receipts WHERE rowid IN (
               SELECT rowid FROM app_server_notification_receipts
               WHERE received_at < ?1 AND length(received_at) = length(?1)
                 AND received_at NOT GLOB '*[^0-9]*'
               LIMIT ?2)",
        )
        .bind(&cutoff)
        .bind(PRUNE_BATCH)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == PRUNE_BATCH as u64)
    }

    /// The exact count and byte cap, which writers may leave slack in
    /// (`prune_replay_on_write`): a read only while under it.
    pub async fn enforce_replay_cap(&self) -> Result<(), sqlx::Error> {
        let (count, bytes): (i64, i64) =
            sqlx::query_as("SELECT event_count, payload_bytes FROM replay_usage WHERE singleton=1")
                .fetch_one(&self.pool)
                .await?;
        if count > REPLAY_MAX_EVENTS || bytes > REPLAY_MAX_BYTES {
            while self.prune_replay_batch().await? {
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }

    /// Streaming writers may leave less than one prune batch of row slack;
    /// the one-second maintenance pass enforces the exact cap
    /// (`enforce_replay_cap`), and a pass every few minutes (and at startup)
    /// the age (`prune_replay`). The O(1) usage read avoids opening a writer
    /// transaction per token.
    /// A byte overflow is still reclaimed immediately, including huge events.
    pub(crate) async fn prune_replay_on_write(&self) -> Result<(), sqlx::Error> {
        let (count, bytes): (i64, i64) =
            sqlx::query_as("SELECT event_count, payload_bytes FROM replay_usage WHERE singleton=1")
                .fetch_one(&self.pool)
                .await?;
        if bytes > REPLAY_MAX_BYTES {
            self.prune_replay().await?;
        } else if count >= REPLAY_MAX_EVENTS + PRUNE_BATCH {
            self.prune_replay_batch().await?;
        }
        Ok(())
    }

    /// A committed event is publishable even if the following maintenance
    /// fails. Reporting a write failure here would suppress delivery and make
    /// retry hit an already-consumed receipt. The periodic cap/age passes
    /// retry cleanup; keep the failure visible without undoing the receipt.
    pub(crate) async fn prune_replay_after_commit(&self) {
        if let Err(error) = self.prune_replay_on_write().await {
            eprintln!("Cupola replay cleanup deferred until maintenance: {error}");
        }
    }

    async fn prune_replay_batch(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // A single statement avoids upgrading a stale read transaction to write.
        let result = sqlx::query("DELETE FROM replay_retention WHERE id IN (SELECT id FROM replay_retention WHERE retained_at < unixepoch() - ? ORDER BY retained_at, id LIMIT ?)")
            .bind(REPLAY_MAX_AGE_SECONDS).bind(PRUNE_BATCH).execute(&mut *tx).await?;
        let age_removed = result.rows_affected();
        let usage =
            sqlx::query("SELECT event_count, payload_bytes FROM replay_usage WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await?;
        let count: i64 = usage.get("event_count");
        let bytes: i64 = usage.get("payload_bytes");
        let overflow = count > REPLAY_MAX_EVENTS || bytes > REPLAY_MAX_BYTES;
        if overflow {
            // Count-only overflow removes only the excess; byte overflow retires
            // one bounded prefix, which may intentionally leave spare capacity.
            let batch = if bytes > REPLAY_MAX_BYTES {
                PRUNE_BATCH
            } else {
                (count - REPLAY_MAX_EVENTS).min(PRUNE_BATCH)
            }
            .min(PRUNE_BATCH - age_removed as i64);
            sqlx::query("DELETE FROM replay_retention WHERE id IN (SELECT id FROM replay_retention ORDER BY id LIMIT ?)")
                .bind(batch).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(age_removed == PRUNE_BATCH as u64 || overflow)
    }

    /// Expired sessions, spent nonces and undeliverable notifications, in
    /// bounded batches, then a bounded return of free pages to the disk.
    pub async fn run_periodic_cleanup(&self, now_ms: i64) -> Result<PeriodicCleanup, sqlx::Error> {
        Ok(PeriodicCleanup {
            expired_sessions: self.prune_expired_sessions(now_ms).await?,
            spent_action_nonces: self.prune_spent_action_nonces(now_ms).await?,
            undeliverable_notifications: self.prune_orphaned_pending_notifications(now_ms).await?,
            reclaimed_bytes: self.reclaim_free_pages().await?,
        })
    }

    /// An expired session is refused already; its row only waits out the
    /// pairing grace before it goes.
    async fn prune_expired_sessions(&self, now_ms: i64) -> Result<u64, sqlx::Error> {
        let cutoff =
            now_ms.saturating_sub(wonder_api::pairing_protocol::PRUNE_AFTER_EXPIRY_MS as i64);
        self.delete_in_batches(
            "DELETE FROM sessions WHERE rowid IN (SELECT rowid FROM sessions WHERE expires_at_ms < CAST(?1 AS INTEGER) LIMIT ?2)",
            &cutoff.to_string(),
        )
        .await
    }

    /// Only wonderd's millisecond claim times are compared, as text of equal
    /// length so the index serves the range.
    async fn prune_spent_action_nonces(&self, now_ms: i64) -> Result<u64, sqlx::Error> {
        let cutoff = (now_ms - ACTION_NONCE_RETENTION_MS).to_string();
        self.delete_in_batches(
            "DELETE FROM action_nonces WHERE rowid IN (
               SELECT rowid FROM action_nonces
               WHERE used_at < ?1 AND length(used_at) = length(?1)
                 AND used_at NOT GLOB '*[^0-9]*'
               LIMIT ?2)",
            &cutoff,
        )
        .await
    }

    async fn delete_in_batches(
        &self,
        statement: &'static str,
        cutoff: &str,
    ) -> Result<u64, sqlx::Error> {
        let mut removed = 0;
        loop {
            let result = sqlx::query(statement)
                .bind(cutoff)
                .bind(PRUNE_BATCH)
                .execute(&self.pool)
                .await?;
            removed += result.rows_affected();
            if result.rows_affected() < PRUNE_BATCH as u64 {
                return Ok(removed);
            }
            tokio::task::yield_now().await;
        }
    }

    /// Databases created before incremental vacuuming keep their free pages;
    /// `convert_to_incremental_vacuum` enables it once at startup.
    async fn reclaim_free_pages(&self) -> Result<i64, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&mut *connection)
            .await?;
        if mode != 2 {
            return Ok(0);
        }
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&mut *connection)
            .await?;
        let free: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&mut *connection)
            .await?;
        let pages = (free - RECLAIM_SLACK_BYTES / page_size).min(RECLAIM_MAX_BYTES / page_size);
        if pages <= 0 {
            return Ok(0);
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA incremental_vacuum({pages})"
        )))
        .execute(&mut *connection)
        .await?;
        let left: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&mut *connection)
            .await?;
        Ok((free - left) * page_size)
    }

    /// Drops held App Server notifications older than the bound that can no
    /// longer be delivered: their thread was never bound to, or no longer
    /// belongs to, a conversation, or it belongs to an archived sub-agent
    /// chat that is not a goal's thread: a week on, nothing replays its buffer.
    /// Rows with a non-numeric receipt time (not written by wonderd) are left
    /// alone.
    pub async fn prune_orphaned_pending_notifications(
        &self,
        now_ms: i64,
    ) -> Result<u64, sqlx::Error> {
        let cutoff = now_ms - ORPHANED_NOTIFICATION_MAX_AGE_MS;
        let mut removed = 0;
        loop {
            let result = sqlx::query(
                "DELETE FROM pending_app_server_notifications WHERE id IN (
                   SELECT p.id FROM pending_app_server_notifications p
                   WHERE p.received_at NOT GLOB '*[^0-9]*' AND p.received_at != ''
                     AND CAST(p.received_at AS INTEGER) < ?
                     AND (p.thread_id IS NULL OR p.thread_id NOT IN (
                       SELECT codex_thread_id FROM conversations WHERE codex_thread_id IS NOT NULL
                       UNION SELECT runtime_thread_id FROM runtime_bindings
                       UNION SELECT codex_thread_id FROM messages WHERE codex_thread_id IS NOT NULL
                       UNION SELECT thread_id FROM goal_threads
                       UNION SELECT thread_id FROM subagent_ownership)
                     OR p.thread_id IN (
                       SELECT thread_id FROM subagent_ownership WHERE is_archived = 1
                       EXCEPT SELECT thread_id FROM goal_threads))
                   LIMIT ?)",
            )
            .bind(cutoff)
            .bind(PRUNE_BATCH)
            .execute(&self.pool)
            .await?;
            removed += result.rows_affected();
            if result.rows_affected() < PRUNE_BATCH as u64 {
                return Ok(removed);
            }
            tokio::task::yield_now().await;
        }
    }

    /// Free pages stay in the file until it is rebuilt once with incremental
    /// vacuuming, which `reclaim_free_pages` then uses. New databases start
    /// that way; an older one is rebuilt here at startup, before anything else
    /// writes, when the disk has room for the rebuild's copies. The rebuild is
    /// atomic: if it fails the database is unchanged and the next start tries
    /// again. Returns whether it ran.
    pub(crate) async fn convert_to_incremental_vacuum(&self) -> Result<bool, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&mut *connection)
            .await?;
        let file: String =
            sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name='main'")
                .fetch_one(&mut *connection)
                .await?;
        if mode != 0 || file.is_empty() {
            return Ok(false);
        }
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&mut *connection)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&mut *connection)
            .await?;
        // VACUUM writes a temporary copy, then the live pages through the WAL.
        let needed = 2 * pages * page_size + VACUUM_DISK_MARGIN_BYTES;
        let directory = std::path::Path::new(&file)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        if available_disk_bytes(directory).is_none_or(|available| available < needed as u64) {
            return Ok(false);
        }
        sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
            .execute(&mut *connection)
            .await?;
        sqlx::query("VACUUM").execute(&mut *connection).await?;
        // Return the WAL the rebuild filled, once.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut *connection)
            .await?;
        Ok(true)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn replay_storage_usage(&self) -> Result<ReplayStorageUsage, sqlx::Error> {
        let row =
            sqlx::query("SELECT event_count, payload_bytes FROM replay_usage WHERE singleton=1")
                .fetch_one(&self.pool)
                .await?;
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let free: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        Ok(ReplayStorageUsage {
            event_count: row.get("event_count"),
            payload_bytes: row.get("payload_bytes"),
            database_bytes: pages * page_size,
            freelist_bytes: free * page_size,
        })
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // The field types differ across platforms.
fn available_disk_bytes(directory: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(directory.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` is written before it is read.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

/// The rebuild is skipped where free space cannot be checked.
#[cfg(not(unix))]
fn available_disk_bytes(_directory: &std::path::Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Once a write commits, failure of optional replay maintenance must not
    // report that the durable event/delta failed. Retrying a delta would find
    // its receipt and suppress the event the caller was never told to publish.
    #[tokio::test]
    async fn cleanup_failure_does_not_disguise_committed_events_and_deltas() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?) INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) SELECT 'epoch','now','messages','update' FROM n")
            .bind(REPLAY_MAX_EVENTS + PRUNE_BATCH - 1)
            .execute(&store.pool).await.unwrap();
        sqlx::query("CREATE TRIGGER refuse_cleanup BEFORE DELETE ON replay_retention BEGIN SELECT RAISE(ABORT,'fixture cleanup failure'); END")
            .execute(&store.pool).await.unwrap();
        let event = |id: &str| -> HostEventEnvelope {
            serde_json::from_value(serde_json::json!({
                "eventId":id,"hostEpoch":"epoch","sequence":0,"occurredAt":"now",
                "conversationId":"chat","threadId":"thread","turnId":"turn","itemId":"item",
                "event":{"type":"assistant_delta","data":{"text":"delta"}}
            }))
            .unwrap()
        };
        let mut published = event("published");
        let commit = store.commit_event(&mut published).await;
        let mut streamed = event("streamed");
        let delta = store
            .upsert_assistant_delta_with_event(
                "chat",
                "thread",
                "turn",
                "item",
                "delta",
                "receipt",
                "now",
                Some(&mut streamed),
            )
            .await;
        let persisted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE event_id IN ('published','streamed')",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(persisted, 2, "both writes actually committed");
        assert!(
            commit.is_ok() && delta.is_ok(),
            "committed writes must succeed: event={commit:?}, delta={delta:?}"
        );
        assert!(
            delta.unwrap().1,
            "the caller must publish the committed delta"
        );
        assert!(streamed.sequence > published.sequence);
        sqlx::query("DROP TRIGGER refuse_cleanup")
            .execute(&store.pool)
            .await
            .unwrap();
        store.enforce_replay_cap().await.unwrap();
        assert!(store.replay_storage_usage().await.unwrap().event_count <= REPLAY_MAX_EVENTS);
        let (message, appended) = store
            .upsert_assistant_delta_with_event(
                "chat",
                "thread",
                "turn",
                "item",
                "delta",
                "receipt",
                "now",
                Some(&mut event("retry")),
            )
            .await
            .unwrap();
        assert!(!appended);
        assert_eq!(message.text, "delta");
    }

    #[tokio::test]
    async fn global_count_bytes_age_and_expiry_preserve_canonical_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("retention.db").display()
        ))
        .await
        .unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        store
            .insert_message("owner", "pending", "unsent text", "hash", "chat", "now")
            .await
            .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100100) INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) SELECT 'epoch','now','messages','update' FROM n").execute(&store.pool).await.unwrap();
        store.prune_replay_on_write().await.unwrap();
        assert_eq!(
            store.replay_storage_usage().await.unwrap().event_count,
            REPLAY_MAX_EVENTS + 100
        );
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<156) INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) SELECT 'epoch','now','messages','update' FROM n").execute(&store.pool).await.unwrap();
        store.prune_replay_on_write().await.unwrap();
        assert_eq!(
            store.replay_storage_usage().await.unwrap().event_count,
            REPLAY_MAX_EVENTS
        );
        store.prune_replay().await.unwrap();
        assert_eq!(
            store.replay_storage_usage().await.unwrap().event_count,
            REPLAY_MAX_EVENTS
        );
        assert!(matches!(
            store.replay_batch("epoch", 0).await.unwrap(),
            ReplayBatch::Resync(_)
        ));
        let usage = store.replay_storage_usage().await.unwrap();
        let huge = "x".repeat(REPLAY_MAX_BYTES as usize + 1);
        // A single oversized record must also be evictable, without special cleanup.
        sqlx::query("INSERT INTO events(event_id,host_epoch,sequence,occurred_at,payload_json) VALUES ('large','older',1,'now',?)").bind(serde_json::json!({"padding":huge}).to_string()).execute(&store.pool).await.unwrap();
        drop(huge);
        store.prune_replay_on_write().await.unwrap();
        let bounded = store.replay_storage_usage().await.unwrap();
        assert!(bounded.payload_bytes <= REPLAY_MAX_BYTES, "{bounded:?}");
        assert!(bounded.event_count <= REPLAY_MAX_EVENTS);
        store.start_event_epoch("new").await.unwrap();
        sqlx::query("INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) VALUES ('new','now','messages','update')").execute(&store.pool).await.unwrap();
        sqlx::query("UPDATE replay_retention SET retained_at=unixepoch()-604801")
            .execute(&store.pool)
            .await
            .unwrap();
        store.prune_replay().await.unwrap();
        let final_usage = store.replay_storage_usage().await.unwrap();
        assert_eq!(final_usage.event_count, 0);
        assert_eq!(final_usage.payload_bytes, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE body='unsent text'")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM history_entries")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            1
        );
        eprintln!("RETENTION_COUNT_CAP={usage:?}; RETENTION_AFTER_BYTES={bounded:?}; RETENTION_AFTER_AGE={final_usage:?}; WAL_BYTES={}",std::fs::metadata(dir.path().join("retention.db-wal")).map(|m|m.len()).unwrap_or(0));
    }

    // The one-second pass enforces the exact cap writers leave slack in, and
    // only reads while under it; the week's age goes with the slower pass.
    #[tokio::test]
    async fn the_cap_pass_trims_to_the_cap_and_leaves_age_to_the_slower_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("cap.db").display()
        ))
        .await
        .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?) INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) SELECT 'epoch','now','messages','update' FROM n")
            .bind(REPLAY_MAX_EVENTS + 100)
            .execute(&store.pool)
            .await
            .unwrap();
        // Within the slack a write leaves.
        store.prune_replay_on_write().await.unwrap();
        let count = || async { store.replay_storage_usage().await.unwrap().event_count };
        assert_eq!(count().await, REPLAY_MAX_EVENTS + 100);
        store.enforce_replay_cap().await.unwrap();
        assert_eq!(count().await, REPLAY_MAX_EVENTS);
        sqlx::query("UPDATE replay_retention SET retained_at=unixepoch()-604801 WHERE id IN (SELECT id FROM replay_retention ORDER BY id LIMIT 10)")
            .execute(&store.pool)
            .await
            .unwrap();
        store.enforce_replay_cap().await.unwrap();
        assert_eq!(
            count().await,
            REPLAY_MAX_EVENTS,
            "under the cap: nothing removed"
        );
        store.prune_replay().await.unwrap();
        assert_eq!(count().await, REPLAY_MAX_EVENTS - 10);
    }

    // Receipts only dedupe a provider redelivering an event already applied;
    // inbox replay releases its own receipt first. Past the replay age they
    // are dropped, in bounded batches; recent and foreign-format ones stay.
    #[tokio::test]
    async fn notification_receipts_expire_with_replay_retention() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("receipts.db").display()
        ))
        .await
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // An hour either side of the cutoff: slow hosts (Wine runs this suite
        // for minutes) must not age "recent" past it before the prune.
        let old = (now - REPLAY_MAX_AGE_SECONDS * 1000 - 3_600_000).to_string();
        let recent = (now - REPLAY_MAX_AGE_SECONDS * 1000 + 3_600_000).to_string();
        for i in 0..(PRUNE_BATCH * 2 + 3) {
            assert!(store
                .claim_app_server_notification(&format!("old-{i}"), &old)
                .await
                .unwrap());
        }
        for (key, at) in [
            ("recent", recent.as_str()),
            ("foreign", "now"),
            ("blank", ""),
        ] {
            assert!(store.claim_app_server_notification(key, at).await.unwrap());
        }
        store.prune_replay().await.unwrap();
        let mut left: Vec<String> =
            sqlx::query_scalar("SELECT notification_key FROM app_server_notification_receipts")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        left.sort();
        assert_eq!(left, ["blank", "foreign", "recent"]);
        // A still-recent receipt keeps deduplicating a redelivery.
        assert!(!store
            .claim_app_server_notification("recent", &now.to_string())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn orphaned_pending_notifications_are_dropped_once_stale() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("pending.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        let now: i64 = 1_800_000_000_000;
        let old = (now - ORPHANED_NOTIFICATION_MAX_AGE_MS - 1).to_string();
        let fresh = (now - 60_000).to_string();
        sqlx::query("INSERT INTO conversations (id, codex_thread_id, created_at) VALUES ('chat', 'bound-thread', 'now')")
            .execute(&store.pool).await.unwrap();
        let mut rows = vec![
            ("bound".to_owned(), Some("bound-thread"), old.clone()),
            ("fresh".to_owned(), Some("gone-thread"), fresh),
            ("not-ms".to_owned(), Some("gone-thread"), "now".to_owned()),
            ("no-thread".to_owned(), None, old.clone()),
        ];
        // More than one delete batch of orphans from a removed Project conversation.
        rows.extend((0..300).map(|i| (format!("orphan-{i}"), Some("gone-thread"), old.clone())));
        for (id, thread, received) in &rows {
            store
                .enqueue_pending_app_server_notification(
                    id,
                    None,
                    "item/completed",
                    *thread,
                    Some("turn"),
                    None,
                    "{}",
                    "{}",
                    received,
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .prune_orphaned_pending_notifications(now)
                .await
                .unwrap(),
            301
        );
        let mut left: Vec<String> =
            sqlx::query_scalar("SELECT id FROM pending_app_server_notifications")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        left.sort();
        assert_eq!(left, ["bound", "fresh", "not-ms"]);
        assert_eq!(
            store
                .prune_orphaned_pending_notifications(now)
                .await
                .unwrap(),
            0
        );
    }

    // Archived sub-agent chats are never routed a buffered row again, so a
    // week on their rows go like other orphans; unarchived children, goal
    // threads and recent rows stay.
    #[tokio::test]
    async fn stale_rows_for_archived_sub_agent_chats_are_retired() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let now: i64 = 1_800_000_000_000;
        let old = (now - ORPHANED_NOTIFICATION_MAX_AGE_MS - 1).to_string();
        let fresh = (now - 60_000).to_string();
        for (child, archived) in [("archived", 1), ("active", 0), ("goal", 1)] {
            sqlx::query(
                "INSERT INTO conversations (id, codex_thread_id, created_at) VALUES (?, ?, 'now')",
            )
            .bind(child)
            .bind(format!("{child}-thread"))
            .execute(&store.pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO subagent_ownership (conversation_id, parent_conversation_id, thread_id, parent_thread_id, source_json, is_archived, verified_at) VALUES (?, ?, ?, 'parent-thread', '{}', ?, 'now')")
                .bind(child).bind(child).bind(format!("{child}-thread")).bind(archived)
                .execute(&store.pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO goal_threads (conversation_id, thread_id) VALUES ('goal', 'goal-thread')",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        for (id, thread, received) in [
            ("archived-old", "archived-thread", &old),
            ("archived-fresh", "archived-thread", &fresh),
            ("active-old", "active-thread", &old),
            ("goal-old", "goal-thread", &old),
        ] {
            store
                .enqueue_pending_app_server_notification(
                    id,
                    None,
                    "item/completed",
                    Some(thread),
                    Some("turn"),
                    None,
                    "{}",
                    "{}",
                    received,
                )
                .await
                .unwrap();
        }
        let cleanup = store.run_periodic_cleanup(now).await.unwrap();
        assert_eq!(cleanup.undeliverable_notifications, 1);
        let mut left: Vec<String> =
            sqlx::query_scalar("SELECT id FROM pending_app_server_notifications")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        left.sort();
        assert_eq!(left, ["active-old", "archived-fresh", "goal-old"]);
    }

    // Expired sessions go once past the pairing grace and spent nonces once
    // no signed action could reuse them, in bounded batches.
    #[tokio::test]
    async fn expired_sessions_and_spent_nonces_are_cleaned_up() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        let now: i64 = 1_800_000_000_000;
        let grace = wonder_api::pairing_protocol::PRUNE_AFTER_EXPIRY_MS as i64;
        for n in 0..(PRUNE_BATCH + 5) {
            store
                .insert_session(
                    &format!("old-{n}"),
                    "owner",
                    "csrf",
                    (now - grace - 1) as u64,
                    "now",
                )
                .await
                .unwrap();
        }
        for (token, expires) in [("in-grace", now - grace + 1_000), ("live", now + 3_600_000)] {
            store
                .insert_session(token, "owner", "csrf", expires as u64, "now")
                .await
                .unwrap();
        }
        for n in 0..(PRUNE_BATCH + 5) {
            assert!(store
                .claim_action_nonce(
                    &format!("spent-{n}"),
                    &(now - ACTION_NONCE_RETENTION_MS - 1).to_string()
                )
                .await
                .unwrap());
        }
        for (nonce, used) in [
            ("recent", (now - 60_000).to_string()),
            ("foreign", "now".to_owned()),
        ] {
            assert!(store.claim_action_nonce(nonce, &used).await.unwrap());
        }
        let cleanup = store.run_periodic_cleanup(now).await.unwrap();
        assert_eq!(cleanup.expired_sessions, PRUNE_BATCH as u64 + 5);
        assert_eq!(cleanup.spent_action_nonces, PRUNE_BATCH as u64 + 5);
        let mut sessions: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        sessions.sort();
        assert_eq!(sessions, ["in-grace", "live"]);
        let mut nonces: Vec<String> = sqlx::query_scalar("SELECT nonce FROM action_nonces")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        nonces.sort();
        assert_eq!(nonces, ["foreign", "recent"]);
        // A recent nonce still refuses a replay.
        assert!(!store
            .claim_action_nonce("recent", &now.to_string())
            .await
            .unwrap());
    }

    // A database made before incremental vacuuming is rebuilt once at startup
    // (giving its free pages back), and later cleanups return free pages
    // above the slack in bounded steps.
    #[tokio::test]
    async fn free_pages_are_returned_to_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vacuum.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pages = |pool: SqlitePool, pragma: &'static str| async move {
            sqlx::query_scalar::<_, i64>(pragma)
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        {
            let legacy = SqlitePoolOptions::new().connect(&url).await.unwrap();
            sqlx::query("PRAGMA journal_mode = WAL")
                .execute(&legacy)
                .await
                .unwrap();
            sqlx::query("CREATE TABLE scratch(blob BLOB)")
                .execute(&legacy)
                .await
                .unwrap();
            sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<4000) INSERT INTO scratch SELECT randomblob(4000) FROM n")
                .execute(&legacy).await.unwrap();
            sqlx::query("DELETE FROM scratch")
                .execute(&legacy)
                .await
                .unwrap();
            assert_eq!(pages(legacy.clone(), "PRAGMA auto_vacuum").await, 0);
            assert!(pages(legacy.clone(), "PRAGMA freelist_count").await > 3000);
            legacy.close().await;
        }
        let store = Store::connect(&url).await.unwrap();
        assert_eq!(pages(store.pool.clone(), "PRAGMA auto_vacuum").await, 2);
        assert!(pages(store.pool.clone(), "PRAGMA freelist_count").await < 100);
        // Already converted: a restart does not rebuild again.
        assert!(!store.convert_to_incremental_vacuum().await.unwrap());

        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<13000) INSERT INTO scratch SELECT randomblob(4000) FROM n")
            .execute(&store.pool).await.unwrap();
        sqlx::query("DELETE FROM scratch")
            .execute(&store.pool)
            .await
            .unwrap();
        let page_size = pages(store.pool.clone(), "PRAGMA page_size").await;
        let free = pages(store.pool.clone(), "PRAGMA freelist_count").await * page_size;
        assert!(free > RECLAIM_SLACK_BYTES + RECLAIM_MAX_BYTES, "{free}");
        let first = store.run_periodic_cleanup(0).await.unwrap().reclaimed_bytes;
        assert_eq!(first, RECLAIM_MAX_BYTES);
        while store.run_periodic_cleanup(0).await.unwrap().reclaimed_bytes > 0 {}
        let left = pages(store.pool.clone(), "PRAGMA freelist_count").await * page_size;
        assert!(
            left <= RECLAIM_SLACK_BYTES && left > RECLAIM_SLACK_BYTES - page_size,
            "{left}"
        );
    }

    #[tokio::test]
    async fn new_databases_start_with_incremental_vacuuming() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("new.db").display()
        ))
        .await
        .unwrap();
        let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(mode, 2);
    }
}
