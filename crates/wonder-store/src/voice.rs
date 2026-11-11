//! Voice sessions on this Mac: the owner's limits, each voice panel the phone
//! opened with the usage it reported, and the text transcripts kept when the
//! owner allows it. wonderd decides what to refuse; this module only stores.

use crate::Store;
use sqlx::Row;

pub const VOICE_SESSION_LIMIT_DEFAULT_MINUTES: u32 = 15;
/// OpenAI ends a Realtime session at 60 minutes; the phone ends it first.
pub const VOICE_SESSION_LIMIT_MAX_MINUTES: u32 = 55;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceSettings {
    pub session_limit_minutes: u32,
    /// Spoken minutes allowed per day on this Mac; `None` is no limit.
    pub daily_minutes: Option<u32>,
    /// Estimated OpenAI cost allowed per day, in cents; `None` is no limit.
    pub daily_cents: Option<u32>,
    pub keep_transcripts: bool,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            session_limit_minutes: VOICE_SESSION_LIMIT_DEFAULT_MINUTES,
            daily_minutes: None,
            daily_cents: None,
            keep_transcripts: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicePanel {
    pub id: String,
    pub device_id: String,
    pub opened_at_ms: i64,
    pub ends_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub mints: u32,
    pub connected_seconds: u32,
    /// Usage events reported for the panel and their summed cost.
    pub usage_events: u32,
    pub cost_microdollars: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceUsageEvent {
    /// `response` or `transcription`.
    pub source: String,
    pub id: String,
    pub cost_microdollars: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceTranscriptEntry {
    pub item_id: String,
    /// `owner` or `voice`.
    pub speaker: String,
    pub text: String,
    pub at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceTranscriptSummary {
    pub panel_id: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub entry_count: u32,
    pub preview: String,
    pub thread_ids: Vec<String>,
}

const PANEL_COLUMNS: &str = "p.id, p.device_id, p.opened_at_ms, p.ends_at_ms, p.ended_at_ms, p.mints, p.connected_seconds, \
     (SELECT COUNT(*) FROM voice_usage_events u WHERE u.panel_id=p.id) AS usage_events, \
     (SELECT COALESCE(SUM(cost_microdollars),0) FROM voice_usage_events u WHERE u.panel_id=p.id) AS cost";

fn panel(row: &sqlx::sqlite::SqliteRow) -> VoicePanel {
    VoicePanel {
        id: row.get("id"),
        device_id: row.get("device_id"),
        opened_at_ms: row.get("opened_at_ms"),
        ends_at_ms: row.get("ends_at_ms"),
        ended_at_ms: row.get("ended_at_ms"),
        mints: row.get::<i64, _>("mints") as u32,
        connected_seconds: row.get::<i64, _>("connected_seconds") as u32,
        usage_events: row.get::<i64, _>("usage_events") as u32,
        cost_microdollars: row.get("cost"),
    }
}

impl Store {
    pub async fn voice_settings(&self) -> Result<VoiceSettings, sqlx::Error> {
        let row = sqlx::query(
            "SELECT session_limit_minutes, daily_minutes, daily_cents, keep_transcripts FROM voice_settings WHERE id=1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map_or_else(VoiceSettings::default, |row| VoiceSettings {
                session_limit_minutes: row.get::<i64, _>("session_limit_minutes") as u32,
                daily_minutes: row.get::<Option<i64>, _>("daily_minutes").map(|v| v as u32),
                daily_cents: row.get::<Option<i64>, _>("daily_cents").map(|v| v as u32),
                keep_transcripts: row.get::<i64, _>("keep_transcripts") != 0,
            }),
        )
    }

    pub async fn set_voice_settings(
        &self,
        settings: &VoiceSettings,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO voice_settings(id,session_limit_minutes,daily_minutes,daily_cents,keep_transcripts,updated_at) VALUES(1,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET session_limit_minutes=excluded.session_limit_minutes, daily_minutes=excluded.daily_minutes, daily_cents=excluded.daily_cents, keep_transcripts=excluded.keep_transcripts, updated_at=excluded.updated_at")
            .bind(i64::from(settings.session_limit_minutes))
            .bind(settings.daily_minutes.map(i64::from))
            .bind(settings.daily_cents.map(i64::from))
            .bind(i64::from(settings.keep_transcripts))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The owner's dictionary, oldest word first.
    pub async fn voice_words(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT word FROM voice_words ORDER BY added_at_ms, rowid")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get("word")).collect())
    }

    /// Adds words; one already present (ignoring case) keeps its place.
    pub async fn add_voice_words(&self, words: &[String], now_ms: i64) -> Result<(), sqlx::Error> {
        for word in words {
            sqlx::query("INSERT OR IGNORE INTO voice_words(word, added_at_ms) VALUES(?, ?)")
                .bind(word)
                .bind(now_ms)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn remove_voice_words(&self, words: &[String]) -> Result<(), sqlx::Error> {
        for word in words {
            sqlx::query("DELETE FROM voice_words WHERE word=?")
                .bind(word)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn voice_panel(&self, id: &str) -> Result<Option<VoicePanel>, sqlx::Error> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {PANEL_COLUMNS} FROM voice_panels p WHERE p.id=?"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(panel))
    }

    /// Panels opened at or after `since_ms`, oldest first.
    pub async fn voice_panels_since(&self, since_ms: i64) -> Result<Vec<VoicePanel>, sqlx::Error> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {PANEL_COLUMNS} FROM voice_panels p WHERE p.opened_at_ms>=? ORDER BY p.opened_at_ms"
        )))
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(panel).collect())
    }

    /// Counts one minted client secret, opening the panel on its first mint.
    /// An existing panel keeps its opening time and end.
    pub async fn record_voice_mint(
        &self,
        id: &str,
        device_id: &str,
        now_ms: i64,
        ends_at_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO voice_panels(id,device_id,opened_at_ms,ends_at_ms,mints) VALUES(?,?,?,?,1) ON CONFLICT(id) DO UPDATE SET mints=mints+1")
            .bind(id)
            .bind(device_id)
            .bind(now_ms)
            .bind(ends_at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The panel closed; no more secrets are minted for it. Returns false
    /// when there is no such panel.
    pub async fn end_voice_panel(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let done =
            sqlx::query("UPDATE voice_panels SET ended_at_ms=COALESCE(ended_at_ms, ?) WHERE id=?")
                .bind(now_ms)
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Adds reported usage. Connected time is the phone's running total, so
    /// the largest report wins; each usage event counts once however often
    /// or late it is reported.
    pub async fn record_voice_usage(
        &self,
        panel_id: &str,
        connected_seconds: u32,
        events: &[VoiceUsageEvent],
    ) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE voice_panels SET connected_seconds=MAX(connected_seconds, ?) WHERE id=?",
        )
        .bind(i64::from(connected_seconds))
        .bind(panel_id)
        .execute(&mut *transaction)
        .await?;
        for event in events {
            sqlx::query("INSERT OR IGNORE INTO voice_usage_events(panel_id,source,event_id,cost_microdollars) VALUES(?,?,?,?)")
                .bind(panel_id)
                .bind(&event.source)
                .bind(&event.id)
                .bind(event.cost_microdollars.max(0))
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await
    }

    /// Saves transcript text for a panel. A repeated item replaces its text
    /// (a later, final transcript), and threads are linked once each.
    pub async fn save_voice_transcript(
        &self,
        panel_id: &str,
        entries: &[VoiceTranscriptEntry],
        thread_ids: &[String],
    ) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        for entry in entries {
            sqlx::query("INSERT INTO voice_transcript_entries(panel_id,item_id,speaker,text,at_ms) VALUES(?,?,?,?,?) ON CONFLICT(panel_id,item_id) DO UPDATE SET speaker=excluded.speaker, text=excluded.text")
                .bind(panel_id)
                .bind(&entry.item_id)
                .bind(&entry.speaker)
                .bind(&entry.text)
                .bind(entry.at_ms)
                .execute(&mut *transaction)
                .await?;
        }
        for thread in thread_ids {
            sqlx::query(
                "INSERT OR IGNORE INTO voice_transcript_threads(panel_id,thread_id) VALUES(?,?)",
            )
            .bind(panel_id)
            .bind(thread)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await
    }

    /// Saved transcripts, newest first, optionally only those that acted on
    /// `thread_id`.
    pub async fn voice_transcripts(
        &self,
        thread_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<VoiceTranscriptSummary>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT p.id, p.opened_at_ms, p.ended_at_ms, \
               (SELECT COUNT(*) FROM voice_transcript_entries e WHERE e.panel_id=p.id) AS entry_count, \
               (SELECT text FROM voice_transcript_entries e WHERE e.panel_id=p.id AND e.speaker='owner' ORDER BY at_ms, item_id LIMIT 1) AS preview \
             FROM voice_panels p \
             WHERE EXISTS (SELECT 1 FROM voice_transcript_entries e WHERE e.panel_id=p.id) \
               AND (?1 IS NULL OR EXISTS (SELECT 1 FROM voice_transcript_threads t WHERE t.panel_id=p.id AND t.thread_id=?1)) \
             ORDER BY p.opened_at_ms DESC, p.id LIMIT ?2",
        )
        .bind(thread_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let panel_id: String = row.get("id");
            let thread_ids = self.voice_transcript_threads(&panel_id).await?;
            summaries.push(VoiceTranscriptSummary {
                started_at_ms: row.get("opened_at_ms"),
                ended_at_ms: row.get("ended_at_ms"),
                entry_count: row.get::<i64, _>("entry_count") as u32,
                preview: row.get::<Option<String>, _>("preview").unwrap_or_default(),
                thread_ids,
                panel_id,
            });
        }
        Ok(summaries)
    }

    async fn voice_transcript_threads(&self, panel_id: &str) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT thread_id FROM voice_transcript_threads WHERE panel_id=? ORDER BY thread_id",
        )
        .bind(panel_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|row| row.get("thread_id")).collect())
    }

    /// One panel's saved transcript in spoken order, with its linked threads;
    /// `None` when nothing was saved for it.
    pub async fn voice_transcript(
        &self,
        panel_id: &str,
    ) -> Result<Option<(VoicePanel, Vec<VoiceTranscriptEntry>, Vec<String>)>, sqlx::Error> {
        let Some(panel) = self.voice_panel(panel_id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT item_id, speaker, text, at_ms FROM voice_transcript_entries WHERE panel_id=? ORDER BY at_ms, item_id",
        )
        .bind(panel_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let entries = rows
            .iter()
            .map(|row| VoiceTranscriptEntry {
                item_id: row.get("item_id"),
                speaker: row.get("speaker"),
                text: row.get("text"),
                at_ms: row.get("at_ms"),
            })
            .collect();
        let threads = self.voice_transcript_threads(panel_id).await?;
        Ok(Some((panel, entries, threads)))
    }

    /// Deletes every saved transcript and its thread links. Usage stays, so
    /// the day's budget still counts those sessions. Returns how many
    /// sessions had a transcript.
    pub async fn delete_voice_transcripts(&self) -> Result<u64, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT panel_id) FROM voice_transcript_entries")
                .fetch_one(&mut *transaction)
                .await?;
        sqlx::query("DELETE FROM voice_transcript_entries")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM voice_transcript_threads")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(sessions as u64)
    }

    /// Forgets usage of panels opened before `before_ms` that have no saved
    /// transcript; budgets only look at the current day.
    pub async fn prune_voice_panels(&self, before_ms: i64) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let stale = "SELECT id FROM voice_panels p WHERE p.opened_at_ms<? AND NOT EXISTS (SELECT 1 FROM voice_transcript_entries e WHERE e.panel_id=p.id)";
        for table in ["voice_usage_events", "voice_transcript_threads"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE panel_id IN ({stale})"
            )))
            .bind(before_ms)
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM voice_panels WHERE id IN ({stale})"
        )))
        .bind(before_ms)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.db").display()
        );
        (dir, Store::connect(&url).await.unwrap())
    }

    fn event(id: &str, cost: i64) -> VoiceUsageEvent {
        VoiceUsageEvent {
            source: "response".into(),
            id: id.into(),
            cost_microdollars: cost,
        }
    }

    // Contract: dictionary words keep their order, a repeat (any case) is
    // ignored, and removing a word leaves the rest.
    #[tokio::test]
    async fn voice_words_add_remove_dedupe() {
        let (_dir, store) = store().await;
        assert!(store.voice_words().await.unwrap().is_empty());
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        store
            .add_voice_words(&words(&["T3", "Cupola"]), 1)
            .await
            .unwrap();
        store
            .add_voice_words(&words(&["t3", "Wonder"]), 2)
            .await
            .unwrap();
        assert_eq!(
            store.voice_words().await.unwrap(),
            ["T3", "Cupola", "Wonder"]
        );
        store.remove_voice_words(&words(&["cupola"])).await.unwrap();
        assert_eq!(store.voice_words().await.unwrap(), ["T3", "Wonder"]);
    }

    // Contract: settings default to a 15-minute session, no daily budget and
    // kept transcripts, and the table refuses a session over 55 minutes.
    #[tokio::test]
    async fn voice_settings_default_and_bounds() {
        let (_dir, store) = store().await;
        assert_eq!(
            store.voice_settings().await.unwrap(),
            VoiceSettings::default()
        );
        let set = VoiceSettings {
            session_limit_minutes: 55,
            daily_minutes: Some(30),
            daily_cents: Some(250),
            keep_transcripts: false,
        };
        store.set_voice_settings(&set, "t").await.unwrap();
        assert_eq!(store.voice_settings().await.unwrap(), set);
        let over = VoiceSettings {
            session_limit_minutes: 56,
            ..set.clone()
        };
        assert!(store.set_voice_settings(&over, "t").await.is_err());
        assert_eq!(store.voice_settings().await.unwrap(), set);
    }

    // Invariant: a panel's usage is the largest connected total reported and
    // the sum of distinct usage events, whatever order, repetition or
    // interleaving with mints and the end the reports arrive in.
    #[tokio::test]
    async fn voice_usage_is_order_and_repeat_independent() {
        #[derive(Clone, Copy, Debug)]
        enum Step {
            Report(u32, &'static [(&'static str, i64)]),
            Mint,
            End,
        }
        use Step::*;
        let a: &[(&str, i64)] = &[("r1", 100)];
        let b: &[(&str, i64)] = &[("r1", 100), ("r2", 50)];
        let c: &[(&str, i64)] = &[("r2", 50)];
        let orderings: &[&[Step]] = &[
            &[Report(10, a), Report(30, b)],
            &[Report(30, b), Report(10, a)],
            &[Report(30, b), Report(30, b)],
            &[Report(10, a), End, Report(30, c)],
            &[Report(30, c), Mint, Report(10, a), Report(10, a)],
            &[End, Report(30, b), End],
            &[Report(10, a), Mint, Report(30, b), Mint],
        ];
        for steps in orderings {
            let (_dir, store) = store().await;
            store
                .record_voice_mint("p", "d", 1_000, 901_000)
                .await
                .unwrap();
            let mut mints = 1;
            for step in *steps {
                match step {
                    Report(seconds, events) => {
                        let events: Vec<_> =
                            events.iter().map(|(id, cost)| event(id, *cost)).collect();
                        store
                            .record_voice_usage("p", *seconds, &events)
                            .await
                            .unwrap();
                    }
                    Mint => {
                        mints += 1;
                        store
                            .record_voice_mint("p", "d", 5_000, 999_999)
                            .await
                            .unwrap();
                    }
                    End => {
                        store.end_voice_panel("p", 2_000).await.unwrap();
                    }
                }
            }
            let panel = store.voice_panel("p").await.unwrap().unwrap();
            assert_eq!(panel.connected_seconds, 30, "{steps:?}");
            assert_eq!(panel.usage_events, 2, "{steps:?}");
            assert_eq!(panel.cost_microdollars, 150, "{steps:?}");
            assert_eq!(panel.mints, mints, "{steps:?}");
            assert_eq!(panel.opened_at_ms, 1_000, "reconnect keeps the start");
            assert_eq!(panel.ends_at_ms, 901_000, "reconnect keeps the end");
        }
    }

    // Contract: transcripts keep the final text per item, link each thread
    // once, filter by thread, and deleting them all keeps the usage.
    #[tokio::test]
    async fn voice_transcripts_link_threads_and_delete_without_usage() {
        let (_dir, store) = store().await;
        store
            .record_voice_mint("p1", "d", 1_000, 9_000)
            .await
            .unwrap();
        store
            .record_voice_mint("p2", "d", 2_000, 9_000)
            .await
            .unwrap();
        store
            .record_voice_usage("p1", 60, &[event("r", 7)])
            .await
            .unwrap();
        let entry = |id: &str, speaker: &str, text: &str, at| VoiceTranscriptEntry {
            item_id: id.into(),
            speaker: speaker.into(),
            text: text.into(),
            at_ms: at,
        };
        store
            .save_voice_transcript(
                "p1",
                &[entry("i1", "owner", "send hi", 1_100)],
                &["chat-a".into()],
            )
            .await
            .unwrap();
        store
            .save_voice_transcript(
                "p1",
                &[
                    entry("i1", "owner", "Send hi to the release chat.", 1_100),
                    entry("i2", "voice", "Sent.", 1_200),
                ],
                &["chat-a".into(), "chat-b".into()],
            )
            .await
            .unwrap();
        store
            .save_voice_transcript("p2", &[entry("j1", "owner", "Hello", 2_100)], &[])
            .await
            .unwrap();

        let all = store.voice_transcripts(None, 10).await.unwrap();
        assert_eq!(
            all.iter().map(|s| s.panel_id.as_str()).collect::<Vec<_>>(),
            ["p2", "p1"]
        );
        assert_eq!(all[1].preview, "Send hi to the release chat.");
        assert_eq!(all[1].entry_count, 2);
        assert_eq!(all[1].thread_ids, ["chat-a", "chat-b"]);
        let linked = store.voice_transcripts(Some("chat-b"), 10).await.unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].panel_id, "p1");
        let (_, entries, threads) = store.voice_transcript("p1").await.unwrap().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].text, "Sent.");
        assert_eq!(threads.len(), 2);

        assert_eq!(store.delete_voice_transcripts().await.unwrap(), 2);
        assert!(store.voice_transcripts(None, 10).await.unwrap().is_empty());
        assert!(store.voice_transcript("p1").await.unwrap().is_none());
        let panel = store.voice_panel("p1").await.unwrap().unwrap();
        assert_eq!((panel.connected_seconds, panel.cost_microdollars), (60, 7));

        store.prune_voice_panels(1_500).await.unwrap();
        assert!(store.voice_panel("p1").await.unwrap().is_none());
        assert!(store.voice_panel("p2").await.unwrap().is_some());
    }
}
