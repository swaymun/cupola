//! Indexed local history, independent of runtime I/O and replay retention.
use super::*;
use wonder_api::{DeliveryState, FILE_CHANGE_PROJECTION_VERSION};

#[derive(Clone, Debug)]
pub struct HistoryCursor {
    pub history_revision: i64,
    pub sort_ms: i64,
    pub sequence: i64,
}

fn native_detail(event: &HostEventEnvelope) -> Option<serde_json::Value> {
    let WonderEvent::Activity {
        category,
        detail: Some(detail),
        ..
    } = &event.event
    else {
        return None;
    };
    if category != "thread_item_upsert" {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(detail).ok()?;
    // Checklists have their own coalescing/history owner. Preserve that
    // path, and never suppress a repair of a live-only observation.
    if value["historyRefresh"] != true || value["item"]["type"] == "taskChecklist" {
        return None;
    }
    value.as_object_mut()?.remove("turnStatusObservedAtMs");
    Some(value)
}

impl Store {
    /// Fetch a durable item through its existing UNIQUE(source, source_id)
    /// index, including threads retained across provider switches. Never scan
    /// a conversation's output or read provider files to expand one row.
    pub async fn conversation_history_item(
        &self,
        host_epoch: &str,
        conversation: &str,
        turn: &str,
        item: &str,
    ) -> Result<Option<(String, Vec<HostEventEnvelope>)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let workspace: Option<String> = sqlx::query_scalar("WITH RECURSIVE parents(id,depth) AS (SELECT ?1,0 UNION ALL SELECT s.parent_conversation_id,p.depth+1 FROM parents p JOIN subagent_ownership s ON s.conversation_id=p.id WHERE p.depth<32) SELECT c.cwd FROM parents p JOIN project_conversations c ON c.conversation_id=p.id WHERE EXISTS(SELECT 1 FROM sync_state WHERE singleton=1 AND host_epoch=?2) LIMIT 1")
            .bind(conversation).bind(host_epoch).fetch_optional(&mut *tx).await?;
        let Some(workspace) = workspace else {
            return Ok(None);
        };
        let rows: Vec<String> = sqlx::query_scalar("WITH threads(id) AS (SELECT COALESCE(codex_thread_id,'') FROM conversations WHERE id=?1 UNION SELECT runtime_thread_id FROM project_runtime_history WHERE conversation_id=?1 AND runtime_thread_id IS NOT NULL UNION SELECT '') SELECT payload_json FROM threads CROSS JOIN history_entries h INDEXED BY sqlite_autoindex_history_entries_1 ON h.source='event' AND h.source_id=?1||':'||threads.id||':'||?2||':'||?3 WHERE h.conversation_id=?1 AND h.hidden_by_reset=0 ORDER BY h.sequence")
            .bind(conversation).bind(turn).bind(item).fetch_all(&mut *tx).await?;
        let mut events = Vec::with_capacity(rows.len());
        for payload in rows {
            let event: HostEventEnvelope = serde_json::from_str(&payload)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            // Delimited legacy keys are not enough to establish ownership.
            if event.conversation_id.as_deref() == Some(conversation)
                && event.turn_id.as_deref() == Some(turn)
                && event.item_id.as_deref() == Some(item)
            {
                events.push(event);
            }
        }
        tx.commit().await?;
        Ok(Some((workspace, events)))
    }

    /// A source checkpoint is valid only after a full, current projection. A
    /// reset/rewind invalidates file_change_version even if the native file is
    /// momentarily unchanged. The caller checks its refresh token separately.
    pub async fn history_source_is_current(
        &self,
        conversation: &str,
        thread: &str,
        revision: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM history_hydration WHERE conversation_id=? AND source_thread=? AND source_revision=? AND file_change_version=?)")
            .bind(conversation).bind(thread).bind(revision).bind(FILE_CHANGE_PROJECTION_VERSION)
            .fetch_one(&self.pool).await
    }

    /// Clear before importing any page; publish only after all pages succeed
    /// and the provider source is still at the revision read at the start.
    pub async fn set_history_source(
        &self,
        conversation: &str,
        token: &str,
        thread: &str,
        revision: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE history_hydration SET source_thread=?,source_revision=? WHERE conversation_id=? AND token=? AND state='refreshing'")
            .bind(thread).bind(revision).bind(conversation).bind(token)
            .execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Native polling is a snapshot read, not a new event for every saved item.
    /// Compare and save one bounded native page in the journal transaction. Only
    /// committed changes are returned for broadcast, in their original order.
    pub async fn commit_history_refresh(
        &self,
        events: Vec<HostEventEnvelope>,
    ) -> Result<Vec<HostEventEnvelope>, sqlx::Error> {
        if events.len() > 100 {
            return Err(sqlx::Error::Protocol(
                "native history page exceeds 100 items".into(),
            ));
        }
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // The common unchanged page needs one indexed read and no writer
        // transaction. This is only a no-op fast path: changed pages still
        // compare under BEGIN IMMEDIATE so concurrent writers and repeated
        // identities within a page retain the journal's atomic contract.
        let keys: Option<Vec<_>> = events.iter().map(|event| {
            Some(serde_json::json!({
                "key": format!("{}:{}:{}:{}", event.conversation_id.as_ref()?,
                    event.thread_id.as_ref()?, event.turn_id.as_ref()?, event.item_id.as_ref()?),
                "conversation": event.conversation_id.as_ref()?, "epoch": event.host_epoch,
            }))
        }).collect();
        if let Some(keys) = keys {
            let saved: Vec<(i64, String)> = sqlx::query_as(
                "SELECT CAST(k.key AS INTEGER),h.payload_json FROM json_each(?) k CROSS JOIN history_entries h INDEXED BY sqlite_autoindex_history_entries_1 ON h.source='event' AND h.source_id=json_extract(k.value,'$.key') AND h.conversation_id=json_extract(k.value,'$.conversation') AND h.hidden_by_reset=0 JOIN sync_state s ON s.singleton=1 AND s.host_epoch=json_extract(k.value,'$.epoch') ORDER BY CAST(k.key AS INTEGER)",
            ).bind(serde_json::Value::Array(keys).to_string()).fetch_all(&self.pool).await?;
            if saved.len() == events.len()
                && saved.iter().all(|(index, payload)| {
                    let Some(detail) = native_detail(&events[*index as usize]) else {
                        return false;
                    };
                    serde_json::from_str::<HostEventEnvelope>(payload)
                        .ok()
                        .and_then(|saved| native_detail(&saved))
                        .as_ref()
                        == Some(&detail)
                })
            {
                return Ok(Vec::new());
            }
        }
        // Compare against the same writer snapshot that saves the page; a
        // concurrent event must not race a deferred read-to-write upgrade.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut committed = Vec::new();
        for mut event in events {
            if let Some(conversation) = &event.conversation_id {
                Self::require_settled_rewind(&mut tx, conversation).await?;
            }
            if let (Some(conversation), Some(thread), Some(turn), Some(item), Some(detail)) = (
                &event.conversation_id,
                &event.thread_id,
                &event.turn_id,
                &event.item_id,
                native_detail(&event),
            ) {
                let key = format!("{conversation}:{thread}:{turn}:{item}");
                let saved: Option<String> = sqlx::query_scalar(
                    "SELECT payload_json FROM history_entries WHERE conversation_id=? AND source='event' AND source_id=? AND hidden_by_reset=0 AND EXISTS(SELECT 1 FROM sync_state WHERE singleton=1 AND host_epoch=?)",
                )
                .bind(conversation)
                .bind(key)
                .bind(&event.host_epoch)
                .fetch_optional(&mut *tx)
                .await?;
                if saved
                    .and_then(|saved| serde_json::from_str::<HostEventEnvelope>(&saved).ok())
                    .and_then(|saved| native_detail(&saved))
                    .as_ref()
                    == Some(&detail)
                {
                    continue;
                }
            }
            self.commit_event_in(&mut tx, &mut event).await?;
            committed.push(event);
        }
        tx.commit().await?;
        if !committed.is_empty() {
            self.prune_replay().await?;
        }
        Ok(committed)
    }

    /// The owning Project directory, including runtime-owned descendants.
    pub async fn conversation_execution_directory(
        &self,
        conversation: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("WITH RECURSIVE parents(id,depth) AS (SELECT ?,0 UNION ALL SELECT s.parent_conversation_id,p.depth+1 FROM parents p JOIN subagent_ownership s ON s.conversation_id=p.id WHERE p.depth<32) SELECT c.cwd FROM parents p JOIN project_conversations c ON c.conversation_id=p.id LIMIT 1")
            .bind(conversation)
            .fetch_optional(&self.pool).await
    }

    /// The history sequences of a conversation's saved thread item events
    /// after `after`, oldest first, whose text could record a pull request
    /// being opened or a branch being pushed. One pass over the conversation;
    /// read the rows with `history_event_details`. Also returns the
    /// conversation's history revision: a native reset changes it and hides
    /// rows, so a reader that remembered an offset starts over.
    pub async fn conversation_pull_request_records(
        &self,
        conversation: &str,
        after: i64,
        limit: u32,
    ) -> Result<(i64, Vec<i64>), sqlx::Error> {
        let revision: i64 = sqlx::query_scalar(
            "SELECT COALESCE((SELECT history_revision FROM conversations WHERE id=?),0)",
        )
        .bind(conversation)
        .fetch_one(&self.pool)
        .await?;
        let sequences = sqlx::query_scalar("SELECT sequence FROM history_entries WHERE conversation_id=?1 AND hidden_by_reset=0 AND source='event' AND sequence>?2 AND (instr(payload_json,'pr create')>0 OR instr(payload_json,'pr new')>0 OR instr(payload_json,'create_pull_request')>0 OR instr(payload_json,' push')>0) AND json_type(payload_json,'$.event.data.detail')='text' ORDER BY sequence LIMIT ?3")
            .bind(conversation).bind(after).bind(i64::from(limit))
            .fetch_all(&self.pool).await?;
        Ok((revision, sequences))
    }

    /// The item event details (the JSON text of `event.data.detail`) of a
    /// conversation's history rows, oldest first.
    pub async fn history_event_details(
        &self,
        conversation: &str,
        sequences: &[i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        let list =
            serde_json::to_string(sequences).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        sqlx::query_as("SELECT sequence, json_extract(payload_json,'$.event.data.detail') FROM history_entries WHERE sequence IN (SELECT value FROM json_each(?2)) AND conversation_id=?1 AND hidden_by_reset=0 AND json_type(payload_json,'$.event.data.detail')='text' ORDER BY sequence")
            .bind(conversation).bind(list)
            .fetch_all(&self.pool).await
    }

    /// How many history rows a conversation has and its newest row: changes
    /// when an item or reply is saved, so derived indexes know to re-read.
    pub async fn conversation_history_mark(
        &self,
        conversation: &str,
    ) -> Result<(i64, i64), sqlx::Error> {
        sqlx::query_as("SELECT COUNT(*), COALESCE(MAX(sequence),0) FROM history_entries WHERE conversation_id=?")
            .bind(conversation)
            .fetch_one(&self.pool)
            .await
    }

    /// A delayed stream item cannot revive a turn hidden by a native reset.
    pub async fn runtime_turn_hidden_by_reset(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rewound_turns WHERE conversation_id=(SELECT conversation_id FROM runtime_bindings WHERE runtime_thread_id=?1) AND thread_id=?1 AND turn_id=?2) OR CASE WHEN (SELECT history_revision FROM conversations WHERE id=(SELECT conversation_id FROM runtime_bindings WHERE runtime_thread_id=?1))>0 THEN EXISTS(SELECT 1 FROM messages m JOIN history_entries h ON h.source='message' AND h.source_id=m.id WHERE m.codex_thread_id=?1 AND m.codex_turn_id=?2 AND h.hidden_by_reset=1 UNION ALL SELECT 1 FROM history_entries WHERE conversation_id=(SELECT conversation_id FROM runtime_bindings WHERE runtime_thread_id=?1) AND source='event' AND hidden_by_reset=1 AND json_extract(payload_json,'$.threadId')=?1 AND json_extract(payload_json,'$.turnId')=?2) ELSE 0 END")
            .bind(thread).bind(turn).fetch_one(&self.pool).await
    }

    pub async fn history_refresh_is_current(
        &self,
        conversation: &str,
        token: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM history_hydration WHERE conversation_id=? AND token=? AND state='refreshing')")
            .bind(conversation).bind(token).fetch_one(&self.pool).await
    }

    pub async fn needs_file_change_repair(
        &self,
        conversation: &str,
        now: i64,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query("SELECT file_change_version,state,updated_at_ms FROM history_hydration WHERE conversation_id=?")
            .bind(conversation).fetch_optional(&self.pool).await?;
        Ok(row.is_none_or(|row| {
            row.get::<i64, _>("file_change_version") < FILE_CHANGE_PROJECTION_VERSION
                && row.get::<i64, _>("updated_at_ms") < now - 60_000
        }))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn committed_conversation_snapshot(
        &self,
        host_epoch: &str,
        conversation_id: &str,
    ) -> Result<Option<CommittedConversationSnapshot>, sqlx::Error> {
        self.conversation_history_page(host_epoch, conversation_id, None, 100)
            .await
    }
    pub async fn conversation_history_page(
        &self,
        host_epoch: &str,
        conversation_id: &str,
        before: Option<&HistoryCursor>,
        limit: u32,
    ) -> Result<Option<CommittedConversationSnapshot>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::require_settled_rewind(&mut tx, conversation_id).await?;
        // The rewind fence and every projection below share this WAL snapshot.
        let sequence: i64 = sqlx::query_scalar(
            "SELECT last_sequence FROM sync_state WHERE singleton = 1 AND host_epoch = ?",
        )
        .bind(host_epoch)
        .fetch_one(&mut *tx)
        .await?;
        let workspace_path: Option<String> = sqlx::query_scalar("WITH RECURSIVE parents(id,depth) AS (SELECT ?,0 UNION ALL SELECT s.parent_conversation_id,p.depth+1 FROM parents p JOIN subagent_ownership s ON s.conversation_id=p.id WHERE p.depth<32) SELECT c.cwd FROM parents p JOIN project_conversations c ON c.conversation_id=p.id LIMIT 1")
            .bind(conversation_id)
            .fetch_optional(&mut *tx).await?;
        let Some(workspace_path) = workspace_path else {
            return Ok(None);
        };
        let history_revision: i64 =
            sqlx::query_scalar("SELECT history_revision FROM conversations WHERE id=?")
                .bind(conversation_id)
                .fetch_one(&mut *tx)
                .await?;
        if before.is_some_and(|cursor| cursor.history_revision != history_revision) {
            return Err(sqlx::Error::Protocol(
                "Conversation history reset; read the newest page".into(),
            ));
        }
        let before_ms = before.map_or(i64::MAX, |c| c.sort_ms);
        let before_sequence = before.map_or(i64::MAX, |c| c.sequence);
        let limit = limit.clamp(1, 100);
        let mut page = sqlx::query("SELECT sequence, sort_ms FROM history_entries WHERE conversation_id = ? AND hidden_by_reset=0 AND (sort_ms, sequence) < (?, ?) ORDER BY sort_ms DESC, sequence DESC LIMIT ?")
            .bind(conversation_id).bind(before_ms).bind(before_sequence).bind(i64::from(limit) + 1)
            .fetch_all(&mut *tx).await?;
        let more = page.len() > limit as usize;
        page.truncate(limit as usize);
        let next_cursor = if more {
            page.last().map(|row| HistoryCursor {
                history_revision,
                sort_ms: row.get("sort_ms"),
                sequence: row.get("sequence"),
            })
        } else {
            None
        };
        let sequences = serde_json::to_string(
            &page
                .iter()
                .map(|row| row.get::<i64, _>("sequence"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        // The page already selected primary keys. Without this hint SQLite
        // can scan every row of the source index before applying the IN list.
        let rows = sqlx::query("SELECT m.* FROM history_entries h NOT INDEXED JOIN messages m ON m.id = h.source_id WHERE h.source = 'message' AND h.sequence IN (SELECT value FROM json_each(?))")
            .bind(&sequences)
            .fetch_all(&mut *tx)
            .await?;
        let mut messages = rows.iter().map(stored_message).collect::<Vec<_>>();
        messages.sort_by(|a, b| {
            compare_timestamps(&a.created_at, &b.created_at).then_with(|| a.id.cmp(&b.id))
        });
        let rows = sqlx::query(
            "SELECT m.* FROM history_entries h NOT INDEXED JOIN assistant_messages m ON m.id = h.source_id WHERE h.source = 'assistant' AND h.sequence IN (SELECT value FROM json_each(?)) ORDER BY m.created_at, m.id",
        )
        .bind(&sequences)
        .fetch_all(&mut *tx)
        .await?;
        let assistant_messages = rows
            .iter()
            .map(stored_assistant_message)
            .collect::<Vec<_>>();
        let rows = sqlx::query("SELECT a.message_id, a.file_id FROM message_attachments a JOIN messages m ON m.id = a.message_id WHERE m.id IN (SELECT source_id FROM history_entries NOT INDEXED WHERE source='message' AND sequence IN (SELECT value FROM json_each(?))) ORDER BY a.file_id")
            .bind(&sequences).fetch_all(&mut *tx).await?;
        let mut attachment_ids = HashMap::<String, Vec<String>>::new();
        for row in rows {
            attachment_ids
                .entry(row.get("message_id"))
                .or_default()
                .push(row.get("file_id"));
        }
        let codex_thread_id =
            sqlx::query_scalar("SELECT codex_thread_id FROM conversations WHERE id = ?")
                .bind(conversation_id)
                .fetch_optional(&mut *tx)
                .await?;
        let rows = sqlx::query("SELECT payload_json FROM history_entries NOT INDEXED WHERE source = 'event' AND sequence IN (SELECT value FROM json_each(?)) ORDER BY sort_ms, sequence")
            .bind(&sequences)
            .fetch_all(&mut *tx)
            .await?;
        let mut events = Vec::new();
        for row in rows {
            let event: HostEventEnvelope =
                serde_json::from_str(&row.get::<String, _>("payload_json"))
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            if event.conversation_id.as_deref() == Some(conversation_id) {
                events.push(event);
            }
        }
        // Item pagination must not paginate away the status of a represented
        // turn. Fetch its two durable boundaries by indexed identity, in this
        // same WAL snapshot, without adding other turns or changing the cursor.
        let turns =
            messages
                .iter()
                .filter_map(|message| {
                    Some((
                        message.codex_thread_id.as_deref()?,
                        message.codex_turn_id.as_deref()?,
                    ))
                })
                .chain(assistant_messages.iter().map(|message| {
                    (
                        message.codex_thread_id.as_str(),
                        message.codex_turn_id.as_str(),
                    )
                }))
                .chain(events.iter().filter_map(|event| {
                    Some((event.thread_id.as_deref()?, event.turn_id.as_deref()?))
                }))
                .collect::<std::collections::HashSet<_>>();
        let mut keys = turns
            .iter()
            .flat_map(|(thread, turn)| {
                ["work-start", "work-end"]
                    .map(|boundary| format!("{conversation_id}:{thread}:{turn}:{boundary}"))
            })
            .collect::<Vec<_>>();
        // A page containing either endpoint must contain its matching endpoint,
        // just as every represented turn already retains both lifecycle boundaries.
        for event in &events {
            if let WonderEvent::Activity {
                category,
                detail: Some(detail),
                ..
            } = &event.event
            {
                if category != "thread_item_upsert" {
                    continue;
                }
                let value: serde_json::Value =
                    serde_json::from_str(detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                if value["item"]["type"] != "reasoning" {
                    continue;
                }
                let base = format!(
                    "{}:{}:{}:{}",
                    conversation_id,
                    event.thread_id.as_deref().unwrap_or(""),
                    value["turnId"].as_str().unwrap_or(""),
                    value["itemId"].as_str().unwrap_or("")
                );
                keys.extend([
                    format!("{base}:thinking-start"),
                    format!("{base}:thinking-end"),
                ]);
            }
        }
        if !keys.is_empty() {
            // This UNIQUE(source, source_id) key owns lifecycle boundaries;
            // the conversation index scans a whole chat for a few identities.
            let rows = sqlx::query("SELECT payload_json FROM history_entries INDEXED BY sqlite_autoindex_history_entries_1 WHERE source='event' AND source_id IN (SELECT value FROM json_each(?)) AND conversation_id=?")
                .bind(serde_json::to_string(&keys).unwrap())
                .bind(conversation_id)
                .fetch_all(&mut *tx)
                .await?;
            let mut seen = events
                .iter()
                .map(|event| event.event_id.clone())
                .collect::<std::collections::HashSet<_>>();
            for row in rows {
                let event: HostEventEnvelope =
                    serde_json::from_str(&row.get::<String, _>("payload_json"))
                        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                if seen.insert(event.event_id.clone()) {
                    events.push(event);
                }
            }
            events.sort_by(|left, right| {
                compare_timestamps(&left.occurred_at, &right.occurred_at)
                    .then_with(|| left.sequence.cmp(&right.sequence))
            });
        }
        tx.commit().await?;
        Ok(Some(CommittedConversationSnapshot {
            workspace_path,
            last_sequence: sequence as u64,
            history_revision,
            messages,
            assistant_messages,
            attachment_ids,
            codex_thread_id,
            events,
            next_cursor,
        }))
    }
    /// Coalesce native plan updates at the existing durable item boundary. This
    /// runs inside the journal transaction, so live replay and restart agree.
    pub(super) async fn prepare_task_history(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        event: &mut HostEventEnvelope,
    ) -> Result<(), sqlx::Error> {
        let Some(conversation) = &event.conversation_id else {
            return Ok(());
        };
        let WonderEvent::Activity {
            category,
            detail: Some(detail),
            ..
        } = &mut event.event
        else {
            return Ok(());
        };
        if category != "thread_item_upsert" {
            return Ok(());
        }
        let mut value: serde_json::Value =
            serde_json::from_str(detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        if value["item"]["type"] != "taskChecklist" || !value["item"]["taskChecklist"].is_object() {
            return Ok(());
        }
        let key = format!(
            "{}:{}:{}:{}",
            conversation,
            event.thread_id.as_deref().unwrap_or(""),
            value["turnId"].as_str().unwrap_or(""),
            value["itemId"].as_str().unwrap_or("")
        );
        let saved: Option<String> = sqlx::query_scalar(
            "SELECT payload_json FROM history_entries WHERE source='event' AND source_id=?",
        )
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
        let previous = saved
            .and_then(|json| serde_json::from_str::<HostEventEnvelope>(&json).ok())
            .and_then(|event| {
                if let WonderEvent::Activity {
                    detail: Some(detail),
                    ..
                } = event.event
                {
                    serde_json::from_str::<serde_json::Value>(&detail).ok()
                } else {
                    None
                }
            });
        // History retains the native receipt stamp, never the read time. A
        // newer native update can advance an observed list; undated/older reads
        // cannot. Live events are already ordered by this journal transaction.
        if value["historyRefresh"] == true {
            if let Some(previous) = &previous {
                let stamp = |item: &serde_json::Value| {
                    item["updatedAt"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                };
                let newer = stamp(&value["item"])
                    .is_some_and(|new| stamp(&previous["item"]).is_none_or(|old| new > old));
                if !newer {
                    value["item"] = previous["item"].clone();
                    *detail = value.to_string();
                    return Ok(());
                }
                // This is a newly observed native update, not a gap-filling
                // read; allow the ordinary history upsert to retain it.
                value["historyRefresh"] = false.into();
            }
        }
        let mut latest = value["item"]["taskChecklist"].clone();
        if let Some(fields) = latest.as_object_mut() {
            fields.remove("history");
        }
        let mut history = previous
            .as_ref()
            .and_then(|value| value["item"]["taskChecklist"]["history"].as_array())
            .cloned()
            .unwrap_or_default();
        if history.is_empty() {
            if let Some(previous) = &previous {
                let mut first = previous["item"]["taskChecklist"].clone();
                if let Some(fields) = first.as_object_mut() {
                    fields.remove("history");
                    history.push(first);
                }
            }
        }
        if history.last() != Some(&latest) {
            history.push(latest);
        }
        if history.len() > 8 {
            history.drain(..history.len() - 8);
        }
        value["item"]["taskChecklist"]["history"] = serde_json::Value::Array(history);
        *detail = value.to_string();
        Ok(())
    }

    pub(super) async fn save_history_event(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        event: &HostEventEnvelope,
    ) -> Result<(), sqlx::Error> {
        let Some(conversation) = &event.conversation_id else {
            return Ok(());
        };
        // Text has its own durable message projection. Keep typed activities once
        // per item so pruning replay cannot erase tool results from the transcript.
        let mut thinking_boundary = false;
        let key = match &event.event {
            WonderEvent::Activity {
                category,
                detail: Some(detail),
                ..
            } if category == "thread_item_upsert" => {
                let value: serde_json::Value =
                    serde_json::from_str(detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                let item_type = value["item"]["type"].as_str().unwrap_or("");
                // Assistant text does not retain phase or other typed metadata.
                // Its lifecycle event must survive even when the text arrived first.
                let existing = if item_type == "userMessage" {
                    sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND client_message_id=?")
                        .bind(conversation).bind(value["item"]["clientId"].as_str().unwrap_or("")).fetch_one(&mut **tx).await? > 0
                } else {
                    false
                };
                if existing {
                    return Ok(());
                }
                let key = format!(
                    "{}:{}:{}:{}",
                    conversation,
                    event.thread_id.as_deref().unwrap_or(""),
                    value["turnId"].as_str().unwrap_or(""),
                    value["itemId"].as_str().unwrap_or(&event.event_id)
                );
                if item_type == "reasoning"
                    && value["lifecycleAuthority"] == 2
                    && value["historyRefresh"] != true
                {
                    // Keep the first two native boundaries with their real clocks.
                    // Ordinary item coalescing deliberately preserves only one clock.
                    thinking_boundary = true;
                    let boundary = if value["state"] == "started" {
                        "start"
                    } else {
                        "end"
                    };
                    format!("{key}:thinking-{boundary}")
                } else {
                    key
                }
            }
            WonderEvent::MessageState { state } if event.turn_id.is_some() => {
                let boundary = match state {
                    DeliveryState::AcceptedByCodex | DeliveryState::Streaming => "work-start",
                    // Uncertain on a turn: it ended with an outcome Wonder
                    // cannot read; the turn stays unknown, not running.
                    DeliveryState::Completed
                    | DeliveryState::Failed
                    | DeliveryState::Interrupted
                    | DeliveryState::Uncertain => "work-end",
                    _ => return Ok(()),
                };
                format!(
                    "{}:{}:{}:{}",
                    conversation,
                    event.thread_id.as_deref().unwrap_or(""),
                    event.turn_id.as_deref().unwrap_or(""),
                    boundary
                )
            }
            WonderEvent::ComputerUseScreenshot { .. } => event.event_id.clone(),
            _ => return Ok(()),
        };
        let history_timestamp = if let WonderEvent::Activity {
            detail: Some(detail),
            ..
        } = &event.event
        {
            let value: serde_json::Value =
                serde_json::from_str(detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            value["item"]["createdAt"].as_str().map(str::to_owned)
        } else {
            None
        };
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT payload_json FROM history_entries WHERE source='event' AND source_id=?",
        )
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await?;
        let previous =
            previous.and_then(|payload| serde_json::from_str::<HostEventEnvelope>(&payload).ok());
        if thinking_boundary && previous.is_some() {
            return Ok(());
        }
        let first_observed = previous.as_ref().map(|saved| saved.occurred_at.clone());
        let timestamp = if thinking_boundary {
            event.occurred_at.clone()
        } else {
            first_observed
                .or(history_timestamp)
                .unwrap_or_else(|| event.occurred_at.clone())
        };
        let mut historical = event.clone();
        historical.occurred_at = timestamp.clone();
        if thinking_boundary {
            if let WonderEvent::Activity {
                detail: Some(detail),
                ..
            } = &mut historical.event
            {
                let mut value: serde_json::Value =
                    serde_json::from_str(detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                // The host already bounded and redacted the thinking summary;
                // it decides at serving time whether the phone sees it.
                if let Some(item) = value["item"].as_object_mut() {
                    item.retain(|key, _| {
                        matches!(key.as_str(), "id" | "type" | "status" | "state" | "summary")
                    });
                }
                if let Some(object) = value.as_object_mut() {
                    object.remove("text");
                }
                *detail = value.to_string();
            }
        }
        let mut refresh = if let WonderEvent::Activity {
            detail: Some(detail),
            ..
        } = &event.event
        {
            serde_json::from_str::<serde_json::Value>(detail)
                .ok()
                .is_some_and(|v| v["historyRefresh"] == true)
        } else {
            false
        };
        // Refresh normally fills gaps without replacing live observations. An
        // older generated-image record may instead contain truncated base64.
        // Enrich only that result with the retained attachment, preserving the
        // original lifecycle, identity and timeline position.
        if refresh {
            if let Some(mut saved) = previous {
                let mut replace = false;
                if let (
                    WonderEvent::Activity {
                        detail: Some(old), ..
                    },
                    WonderEvent::Activity {
                        detail: Some(new), ..
                    },
                ) = (&mut saved.event, &event.event)
                {
                    let mut old_value: serde_json::Value =
                        serde_json::from_str(old).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                    let new_value: serde_json::Value =
                        serde_json::from_str(new).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                    // Project history is provider-owned: an entry that only
                    // ever came from a native read takes the newer native read
                    // (a turn continued or finished on the Mac), keeping its
                    // original position. Live observations are never replaced.
                    if new_value["turnStatus"].is_string()
                        && old_value["historyRefresh"] == true
                        && old_value != new_value
                    {
                        replace = true;
                        refresh = false;
                    }
                    if old_value["item"]["type"] == "contextCompaction"
                        && new_value["item"]["type"] == "contextCompaction"
                        && new_value["state"] == "completed"
                        && new_value["lifecycleAuthority"] == 2
                        && matches!(
                            old_value["state"].as_str(),
                            Some("started" | "inProgress" | "unknown")
                        )
                    {
                        // The producer's item completion timestamp repairs a
                        // missed item/completed event. Keep the observed start,
                        // position and turn receipt; never replace an explicit
                        // failure/interruption or infer from the parent turn.
                        old_value["state"] = "completed".into();
                        old_value["lifecycleAuthority"] = 2.into();
                        *old = old_value.to_string();
                        refresh = false;
                    }
                    if old_value["item"]["type"] == "fileChange"
                        && new_value["item"]["type"] == "fileChange"
                        && old_value["item"]["fileChangeVersion"] != FILE_CHANGE_PROJECTION_VERSION
                        && new_value["item"]["fileChangeVersion"] == FILE_CHANGE_PROJECTION_VERSION
                    {
                        // Repair presentation fields only. Keep the original
                        // item lifecycle, timestamp and cursor identity.
                        for key in [
                            "paths",
                            "diffs",
                            "additions",
                            "deletions",
                            "fileChangeVersion",
                        ] {
                            old_value["item"][key] = new_value["item"][key].clone();
                        }
                        *old = old_value.to_string();
                        refresh = false;
                    }
                    if old_value["item"]["type"] == "imageGeneration"
                        && new_value["item"]["type"] == "imageGeneration"
                        && (old_value["item"]["result"].is_string()
                            || old_value["item"]["result"].is_null())
                        && new_value["item"]["result"]["content"][0]["type"] == "wonderArtifact"
                    {
                        old_value["item"]["result"] = new_value["item"]["result"].clone();
                        old_value["item"]
                            .as_object_mut()
                            .unwrap()
                            .remove("savedPath");
                        *old = old_value.to_string();
                        refresh = false;
                    }
                }
                if !refresh && !replace {
                    historical = saved;
                }
            }
        }
        let payload =
            serde_json::to_string(&historical).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query("INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json) VALUES (?,'event',?,CASE WHEN ? NOT GLOB '*[^0-9]*' THEN CAST(? AS INTEGER) ELSE COALESCE(CAST((julianday(?) - 2440587.5)*86400000 AS INTEGER),0) END,?) ON CONFLICT(source,source_id) DO UPDATE SET payload_json=excluded.payload_json WHERE ? = 0")
            .bind(conversation).bind(key).bind(&timestamp).bind(&timestamp).bind(&timestamp).bind(payload).bind(refresh).execute(&mut **tx).await?;
        Ok(())
    }
}

impl Store {
    pub async fn claim_history_refresh(
        &self,
        conversation: &str,
        token: &str,
        now: i64,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=?)",
        )
        .bind(conversation)
        .fetch_one(&mut *tx)
        .await?;
        if pending {
            return Ok(false);
        }
        let result = sqlx::query("INSERT INTO history_hydration(conversation_id,state,updated_at_ms,detail,token) VALUES (?,'refreshing',?,NULL,?) ON CONFLICT(conversation_id) DO UPDATE SET state='refreshing', updated_at_ms=excluded.updated_at_ms, detail=NULL, token=excluded.token WHERE history_hydration.state != 'refreshing' OR history_hydration.updated_at_ms < ?")
            .bind(conversation).bind(now).bind(token).bind(now - 60_000).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn update_history_refresh(
        &self,
        conversation: &str,
        token: &str,
        state: &str,
        now: i64,
        detail: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE history_hydration SET file_change_version=CASE WHEN ?='completed' THEN ? ELSE file_change_version END,state=?,updated_at_ms=?,detail=? WHERE conversation_id=? AND token=?")
            .bind(state).bind(FILE_CHANGE_PROJECTION_VERSION).bind(state).bind(now).bind(detail).bind(conversation).bind(token).execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn history_refresh_status(
        &self,
        conversation: &str,
        now: i64,
    ) -> Result<serde_json::Value, sqlx::Error> {
        let row = sqlx::query(
            "SELECT state,updated_at_ms,detail FROM history_hydration WHERE conversation_id=?",
        )
        .bind(conversation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            None => serde_json::json!({"state":"idle","detail":null}),
            Some(row) => {
                let state: String = row.get("state");
                let stale =
                    state == "refreshing" && row.get::<i64, _>("updated_at_ms") < now - 60_000;
                serde_json::json!({"state":if stale { "failed" } else { &state }, "detail": if stale { Some("History refresh stopped. Try refreshing again.".to_owned()) } else { row.get::<Option<String>,_>("detail") }})
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn compaction_native_completion_repairs_missed_event_without_replacing_failures() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        seed_project_thread(&store, "chat", "Project", "/tmp", None, None, "now")
            .await
            .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        for original_state in ["started", "unknown", "failed", "interrupted", "completed"] {
            let id = format!("compaction-{original_state}");
            let mut original = serde_json::json!({"turnId":"turn","itemId":id,"state":original_state,"lifecycleAuthority":2,
                "item":{"id":id,"type":"contextCompaction"}});
            let mut event: HostEventEnvelope = serde_json::from_value(serde_json::json!({
                "eventId":id,"hostEpoch":"epoch","sequence":0,"occurredAt":"1700000000000",
                "conversationId":"chat","threadId":"thread","turnId":"turn","itemId":id,
                "event":{"type":"activity","data":{"category":"thread_item_upsert","state":"updated","detail":original.to_string()}}
            })).unwrap();
            store.commit_event(&mut event).await.unwrap();
            original["historyRefresh"] = true.into();
            original["state"] = "completed".into();
            event.event_id.push_str("-hydrated");
            event.occurred_at = "1700000005000".into();
            if let WonderEvent::Activity { detail, .. } = &mut event.event {
                *detail = Some(original.to_string());
            }
            store.commit_event(&mut event).await.unwrap();
        }
        let page = store
            .conversation_history_page("epoch", "chat", None, 20)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.events.len(), 5);
        for saved in page.events {
            let WonderEvent::Activity {
                detail: Some(detail),
                ..
            } = saved.event
            else {
                panic!("activity")
            };
            let value: serde_json::Value = serde_json::from_str(&detail).unwrap();
            let expected = match value["itemId"].as_str().unwrap() {
                "compaction-failed" => "failed",
                "compaction-interrupted" => "interrupted",
                _ => "completed",
            };
            assert_eq!(value["state"], expected);
            assert_eq!(
                saved.occurred_at, "1700000000000",
                "timeline anchor is preserved"
            );
        }
    }

    #[tokio::test]
    async fn native_refresh_replays_changes_without_replaying_unchanged_history() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("history.db").display()
        ))
        .await
        .unwrap();
        seed_project_thread(&store, "chat", "Project", "/tmp", None, None, "now")
            .await
            .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        let make = |index: u64, text: &str, status: &str| -> HostEventEnvelope {
            let detail = serde_json::json!({"turnId":"turn","itemId":"reply","state":"completed","lifecycleAuthority":1,
                "historyRefresh":true,"turnStatus":status,"turnStatusObservedAtMs":index,
                "item":{"id":"reply","type":"agentMessage","text":text}});
            serde_json::from_value(serde_json::json!({"eventId":format!("read-{index}"),"hostEpoch":"epoch","sequence":0,
                "occurredAt":format!("{}",1700000000000_u64+index),"conversationId":"chat","threadId":"thread","turnId":"turn","itemId":"reply",
                "event":{"type":"activity","data":{"category":"thread_item_upsert","state":"updated","detail":detail.to_string()}}})).unwrap()
        };
        let first = store
            .commit_history_refresh(vec![make(1, "Working", "inProgress")])
            .await
            .unwrap()
            .remove(0);
        // An unchanged native page can read the WAL while another connection
        // owns the writer. It must not queue behind control/ingestion writes.
        let writer = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(store
            .commit_history_refresh(vec![make(2, "Working", "inProgress")])
            .await
            .unwrap()
            .is_empty());
        writer.rollback().await.unwrap();
        for index in 2..20 {
            assert!(store
                .commit_history_refresh(vec![make(index, "Working", "inProgress")])
                .await
                .unwrap()
                .is_empty());
        }
        let snapshot = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.last_sequence, first.sequence,
            "unchanged reads do not advance replay"
        );
        let changed = store
            .commit_history_refresh(vec![make(20, "Updated desktop reply", "inProgress")])
            .await
            .unwrap()
            .remove(0);
        assert_eq!(changed.sequence, first.sequence + 1);
        let complete = store
            .commit_history_refresh(vec![make(21, "Updated desktop reply", "completed")])
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            complete.sequence,
            changed.sequence + 1,
            "native lifecycle transitions still replay"
        );
        let snapshot = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.events.len(),
            1,
            "history keeps stable item identity"
        );
        assert_eq!(snapshot.events[0].occurred_at, first.occurred_at);
        let mut wrong_epoch = make(22, "Updated desktop reply", "completed");
        wrong_epoch.host_epoch = "retired".into();
        assert!(store
            .commit_history_refresh(vec![
                make(22, "Must roll back with the page", "completed"),
                wrong_epoch
            ])
            .await
            .is_err());
        assert_eq!(
            store.committed_sequence("epoch").await.unwrap(),
            complete.sequence
        );
        let unchanged = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            unchanged.events[0], snapshot.events[0],
            "failed page leaves neither replay nor history changes"
        );
        let page = store
            .commit_history_refresh(vec![
                make(24, "Updated desktop reply", "completed"),
                make(25, "First page update", "completed"),
                make(26, "First page update", "completed"),
                make(27, "Second page update", "completed"),
            ])
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(),
            ["read-25", "read-27"]
        );
        assert_eq!(page[0].sequence, complete.sequence + 1);
        assert_eq!(page[1].sequence, complete.sequence + 2);
        let after_page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_page.events.len(), 1);
        assert_eq!(after_page.events[0].occurred_at, first.occurred_at);
        assert_eq!(after_page.events[0].event, page[1].event);
        assert!(store
            .commit_history_refresh(
                (0..101)
                    .map(|i| make(100 + i, "Oversized", "completed"))
                    .collect()
            )
            .await
            .is_err());
        assert_eq!(
            store.committed_sequence("epoch").await.unwrap(),
            page[1].sequence
        );
        let mut live = make(23, "Live observation", "completed");
        store.commit_event(&mut live).await.unwrap();
        assert_eq!(
            live.sequence,
            page[1].sequence + 1,
            "ordinary events retain their journal contract"
        );
    }

    // The existing journal/history transaction owns bounded updates, replay,
    // paging and restart. No native read may replace observed progress.
    #[tokio::test]
    async fn task_history_is_bounded_deduplicated_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("tasks.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        seed_project_thread(&store, "chat", "Project", "/tmp", None, None, "now")
            .await
            .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        let mut final_event = None;
        for update in 0..12 {
            for duplicate in 0..2 {
                let mut detail = serde_json::json!({"turnId":"turn","itemId":"turn:tasks","state":"completed","lifecycleAuthority":2,
                    "historyRefresh":update == 0 && duplicate == 0,
                    "item":{"id":"turn:tasks","type":"taskChecklist","status":"completed","updatedAt":format!("{}",1000+update),
                        "taskChecklist":{"entries":[{"text":format!("Task {update}"),"status":"pending"}],"total":1,"completed":0}}});
                if update == 0 {
                    detail["item"].as_object_mut().unwrap().remove("updatedAt");
                }
                let mut event: HostEventEnvelope = serde_json::from_value(serde_json::json!({
                    "eventId":format!("{update}-{duplicate}"),"hostEpoch":"epoch","sequence":0,"occurredAt":format!("{}",1700000000000_i64+update),
                    "conversationId":"chat","threadId":"thread","turnId":"turn","itemId":"turn:tasks",
                    "event":{"type":"activity","data":{"category":"thread_item_upsert","state":"updated","detail":detail.to_string()}}
                })).unwrap();
                store.commit_event(&mut event).await.unwrap();
                if update == 1 {
                    let WonderEvent::Activity {
                        detail: Some(detail),
                        ..
                    } = &event.event
                    else {
                        panic!("typed item")
                    };
                    let value: serde_json::Value = serde_json::from_str(detail).unwrap();
                    assert_eq!(
                        value["item"]["taskChecklist"]["history"]
                            .as_array()
                            .unwrap()
                            .len(),
                        2,
                        "The first hydrated snapshot survives its first live update"
                    );
                }
                final_event = Some(event);
            }
        }
        let item = |event: &HostEventEnvelope| -> serde_json::Value {
            let WonderEvent::Activity {
                detail: Some(detail),
                ..
            } = &event.event
            else {
                panic!("typed item")
            };
            serde_json::from_str::<serde_json::Value>(detail).unwrap()["item"].clone()
        };
        let latest = item(&final_event.unwrap());
        assert_eq!(
            latest["taskChecklist"]["history"].as_array().unwrap().len(),
            8
        );
        assert_eq!(
            latest["taskChecklist"]["history"][0]["entries"][0]["text"],
            "Task 4"
        );
        store.pool.close().await;
        let reopened = Store::connect(&url).await.unwrap();
        let page = reopened
            .conversation_history_page("epoch", "chat", None, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(item(&page.events[0]), latest);
        let mut refresh = page.events[0].clone();
        refresh.event_id = "refresh".into();
        if let WonderEvent::Activity {
            detail: Some(detail),
            ..
        } = &mut refresh.event
        {
            let mut value: serde_json::Value = serde_json::from_str(detail).unwrap();
            value["historyRefresh"] = true.into();
            value["item"]["taskChecklist"] =
                serde_json::json!({"entries":[],"total":0,"completed":0});
            *detail = value.to_string();
        }
        reopened.commit_event(&mut refresh).await.unwrap();
        assert_eq!(
            item(&refresh),
            latest,
            "Even live refresh replay retains the accepted native list"
        );
        refresh.event_id = "newer-native-receipt".into();
        if let WonderEvent::Activity {
            detail: Some(detail),
            ..
        } = &mut refresh.event
        {
            let mut value: serde_json::Value = serde_json::from_str(detail).unwrap();
            value["historyRefresh"] = true.into();
            value["item"]["updatedAt"] = "2000".into();
            value["item"]["taskChecklist"] = serde_json::json!({"entries":[{"text":"Confirmed after restart","status":"completed"}],"total":1,"completed":1});
            *detail = value.to_string();
        }
        reopened.commit_event(&mut refresh).await.unwrap();
        assert_eq!(item(&refresh)["taskChecklist"]["completed"], 1);
        assert_eq!(
            item(&refresh)["taskChecklist"]["history"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        let page = reopened
            .conversation_history_page("epoch", "chat", None, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            item(&page.events[0]),
            item(&refresh),
            "Newer native hydration advances the durable snapshot"
        );

        assert_eq!(
            reopened
                .conversation_history_page("epoch", "chat", None, 1)
                .await
                .unwrap()
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn image_refresh_enriches_legacy_result_without_replacing_live_history() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let original = serde_json::json!({"turnId":"turn","itemId":"image","state":"failed",
            "item":{"id":"image","type":"imageGeneration","status":"failed","result":"truncated base64",
                "savedPath":"/private/image.png","failure":"original failure"}});
        let mut event: HostEventEnvelope = serde_json::from_value(serde_json::json!({
            "eventId":"original","hostEpoch":"epoch","sequence":1,"occurredAt":"1700000000001",
            "conversationId":"chat","threadId":"thread","turnId":"turn","itemId":"image",
            "event":{"type":"activity","data":{"category":"thread_item_upsert","state":"updated","detail":original.to_string()}}
        })).unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        store.save_history_event(&mut tx, &event).await.unwrap();
        tx.commit().await.unwrap();
        let before: (i64, i64) = sqlx::query_as("SELECT sequence,sort_ms FROM history_entries")
            .fetch_one(&store.pool)
            .await
            .unwrap();

        event.event_id = "refreshed".into();
        event.occurred_at = "1800000000000".into();
        let result = serde_json::json!({"content":[{"type":"wonderArtifact","file":{"id":"verified-file"}}]});
        let mut refreshed = original.clone();
        refreshed["historyRefresh"] = true.into();
        refreshed["state"] = "completed".into();
        refreshed["item"]["status"] = "completed".into();
        refreshed["item"]["failure"] = serde_json::Value::Null;
        refreshed["item"]["result"] = result.clone();
        if let WonderEvent::Activity { detail, .. } = &mut event.event {
            *detail = Some(refreshed.to_string());
        }
        for _ in 0..2 {
            let mut tx = store.pool.begin().await.unwrap();
            store.save_history_event(&mut tx, &event).await.unwrap();
            tx.commit().await.unwrap();
        }
        let rows: Vec<(i64, i64, String)> =
            sqlx::query_as("SELECT sequence,sort_ms,payload_json FROM history_entries")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1), before);
        let saved: HostEventEnvelope = serde_json::from_str(&rows[0].2).unwrap();
        assert_eq!(saved.event_id, "original");
        assert_eq!(saved.occurred_at, "1700000000001");
        let WonderEvent::Activity {
            detail: Some(detail),
            ..
        } = saved.event
        else {
            panic!("Expected image activity")
        };
        let mut expected = original;
        expected["item"]["result"] = result;
        expected["item"]
            .as_object_mut()
            .unwrap()
            .remove("savedPath");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&detail).unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn ten_thousand_items_paginate_without_gaps_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("history.db").display()
        );
        let store = Store::connect(&url).await.unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        seed_project_thread(&store, "chat", "Project", "/tmp", None, None, "now")
            .await
            .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) SELECT printf('message-%05d',x),'owner',printf('client-%05d',x),printf('%01024d',x),'hash','chat','completed',CAST(1700000000000+x/3 AS TEXT) FROM n").execute(&store.pool).await.unwrap();
        let cold = Instant::now();
        store
            .conversation_history_page("epoch", "chat", None, 100)
            .await
            .unwrap();
        let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
        let mut samples = vec![];
        for i in 0..105 {
            let start = Instant::now();
            let page = store
                .conversation_history_page("epoch", "chat", None, 100)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(page.messages.len(), 100);
            if i >= 5 {
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        eprintln!("HISTORY_PAGE_COLD_MS={cold_ms}; HISTORY_PAGE_SAMPLES_MS={samples:?}");
        samples.sort_by(f64::total_cmp);
        assert!(samples[94] <= 200.0, "history p95 {}", samples[94]);
        let plan: Vec<String> = sqlx::query("EXPLAIN QUERY PLAN SELECT sequence,sort_ms FROM history_entries WHERE conversation_id='chat' AND (sort_ms,sequence)<(1700000003000,9999) ORDER BY sort_ms DESC,sequence DESC LIMIT 101").fetch_all(&store.pool).await.unwrap().iter().map(|r|r.get::<String,_>("detail")).collect();
        assert!(
            plan.iter().any(|s| s.contains("history_conversation_page")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|s| s.contains("TEMP B-TREE")), "{plan:?}");
        let mut before = None;
        let mut ids = std::collections::HashSet::new();
        for page_index in 0..100 {
            let page = store
                .conversation_history_page("epoch", "chat", before.as_ref(), 100)
                .await
                .unwrap()
                .unwrap();
            for message in page.messages {
                assert!(ids.insert(message.id));
            }
            before = page.next_cursor;
            if page_index == 0 {
                store
                    .insert_message("owner", "new", "new", "hash", "chat", "1800000000000")
                    .await
                    .unwrap();
            }
            if page_index < 99 {
                assert!(before.is_some());
            }
        }
        assert!(before.is_none());
        assert_eq!(ids.len(), 10000);
        for i in 0..10 {
            let reopened = Store::connect(&url).await.unwrap();
            reopened
                .start_event_epoch(&format!("restart-{i}"))
                .await
                .unwrap();
            assert_eq!(
                reopened
                    .conversation_history_page(&format!("restart-{i}"), "chat", None, 100)
                    .await
                    .unwrap()
                    .unwrap()
                    .messages
                    .len(),
                100
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM history_entries")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            10001
        );
        eprintln!(
            "HISTORY_STORAGE={:?}; WAL_BYTES={}",
            store.replay_storage_usage().await.unwrap(),
            std::fs::metadata(dir.path().join("history.db-wal"))
                .map(|m| m.len())
                .unwrap_or(0)
        );
    }

    #[tokio::test]
    async fn restart_and_replay_expiry_preserve_typed_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("upgrade.db").display()
        );
        let old = Store::connect(&url).await.unwrap();
        seed_project_thread(&old, "chat", "Project", "/tmp", None, None, "now")
            .await
            .unwrap();
        old.upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        old.start_event_epoch("epoch").await.unwrap();
        sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) VALUES('pending','owner','pending','unsent','hash','chat','accepted_by_wonder','1700000000000')")
            .execute(&old.pool).await.unwrap();
        let mut event=HostEventEnvelope {event_id:"tool".into(),host_epoch:"epoch".into(),sequence:1,occurred_at:"1700000000001".into(),conversation_id:Some("chat".into()),thread_id:Some("thread".into()),turn_id:Some("turn".into()),item_id:Some("command".into()),request_id:None,device_id:None,message_id:None,approval_id:None,event:WonderEvent::Activity {category:"thread_item_upsert".into(),state:"updated".into(),detail:Some(serde_json::json!({"itemId":"command","turnId":"turn","state":"completed","item":{"id":"command","type":"commandExecution","command":"echo saved","status":"completed"}}).to_string())}};
        old.commit_event(&mut event).await.unwrap();
        old.pool.close().await;
        let store = Store::connect(&url).await.unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.messages[0].body, "unsent");
        assert_eq!(page.events.len(), 1);
        sqlx::query("UPDATE replay_retention SET retained_at=0")
            .execute(&store.pool)
            .await
            .unwrap();
        store.prune_replay().await.unwrap();
        assert_eq!(
            store
                .committed_conversation_snapshot("epoch", "chat")
                .await
                .unwrap()
                .unwrap()
                .events
                .len(),
            1
        );
        let mut refresh = event.clone();
        if let WonderEvent::Activity { detail, .. } = &mut refresh.event {
            let mut payload: serde_json::Value =
                serde_json::from_str(detail.as_ref().unwrap()).unwrap();
            payload["historyRefresh"] = true.into();
            payload["item"]["command"] = "stale".into();
            *detail = Some(payload.to_string());
        }
        let mut tx = store.pool.begin().await.unwrap();
        store.save_history_event(&mut tx, &refresh).await.unwrap();
        tx.commit().await.unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(!serde_json::to_string(&page.events)
            .unwrap()
            .contains("stale"));
        let mut imported = event.clone();
        imported.item_id = Some("imported".into());
        imported.turn_id = Some("import-turn".into());
        imported.event=WonderEvent::Activity {category:"thread_item_upsert".into(),state:"updated".into(),detail:Some(serde_json::json!({"historyRefresh":true,"turnId":"import-turn","itemId":"imported","item":{"type":"agentMessage","id":"imported","text":"imported text","phase":"commentary"}}).to_string())};
        let mut tx = store.pool.begin().await.unwrap();
        store.save_history_event(&mut tx, &imported).await.unwrap();
        tx.commit().await.unwrap();
        store
            .complete_assistant_message(
                "chat",
                "thread",
                "import-turn",
                "imported",
                "live text",
                "1700000000002",
            )
            .await
            .unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            page.events.len(),
            2,
            "canonical text preserves typed phase metadata"
        );
        assert_eq!(page.assistant_messages.len(), 1);
        assert_eq!(page.assistant_messages[0].text, "live text");
        // The reverse order must retain metadata too: text exists before a late event.
        let mut tx = store.pool.begin().await.unwrap();
        store.save_history_event(&mut tx, &imported).await.unwrap();
        tx.commit().await.unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(serde_json::to_string(&page.events)
            .unwrap()
            .contains("commentary"));

        // Streaming updates retain the first observed start, independently of replay retention.
        let mut tx = store.pool.begin().await.unwrap();
        let mut boundary = event.clone();
        boundary.item_id = None;
        for (timestamp, state) in [
            ("1700000000100", DeliveryState::AcceptedByCodex),
            ("1700000000200", DeliveryState::Streaming),
            ("1700000001100", DeliveryState::Completed),
        ] {
            boundary.occurred_at = timestamp.into();
            boundary.event = WonderEvent::MessageState { state };
            store.save_history_event(&mut tx, &boundary).await.unwrap();
        }
        tx.commit().await.unwrap();
        store.prune_replay().await.unwrap();
        let page = store
            .committed_conversation_snapshot("epoch", "chat")
            .await
            .unwrap()
            .unwrap();
        let times: Vec<_> = page
            .events
            .iter()
            .filter(|event| matches!(event.event, WonderEvent::MessageState { .. }))
            .map(|event| event.occurred_at.as_str())
            .collect();
        assert_eq!(times, ["1700000000100", "1700000001100"]);

        store.reset_workspace("owner", "now").await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM history_entries")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn stale_refresh_can_be_retried_without_cleanup_and_old_job_cannot_finish_new_job() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert!(store
            .claim_history_refresh("chat", "old", 1000)
            .await
            .unwrap());
        assert!(!store
            .claim_history_refresh("chat", "other", 1001)
            .await
            .unwrap());
        // A previously completed projection still needs native move metadata.
        sqlx::query(
            "UPDATE history_hydration SET file_change_version=1 WHERE conversation_id='chat'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(!store.needs_file_change_repair("chat", 1001).await.unwrap());
        assert!(store.needs_file_change_repair("chat", 62000).await.unwrap());
        assert_eq!(
            store.history_refresh_status("chat", 62000).await.unwrap()["state"],
            "failed"
        );
        assert!(store
            .claim_history_refresh("chat", "new", 62000)
            .await
            .unwrap());
        assert!(!store
            .set_history_source("chat", "old", "thread", Some("old-source"))
            .await
            .unwrap());
        assert!(store
            .set_history_source("chat", "new", "thread", Some("new-source"))
            .await
            .unwrap());
        assert!(
            !store
                .history_source_is_current("chat", "thread", "new-source")
                .await
                .unwrap(),
            "a bootstrap or obsolete projection has not completed yet"
        );
        assert!(!store
            .update_history_refresh("chat", "old", "completed", 62001, None)
            .await
            .unwrap());
        assert!(store
            .update_history_refresh("chat", "new", "completed", 62001, None)
            .await
            .unwrap());
        assert!(!store
            .needs_file_change_repair("chat", 200_000)
            .await
            .unwrap());
        assert!(store
            .history_source_is_current("chat", "thread", "new-source")
            .await
            .unwrap());
        assert!(!store
            .history_source_is_current("chat", "other-thread", "new-source")
            .await
            .unwrap());
        assert!(!store
            .history_source_is_current("other-chat", "thread", "new-source")
            .await
            .unwrap());
        assert!(
            !store
                .set_history_source("chat", "new", "thread", Some("late-source"))
                .await
                .unwrap(),
            "a completed job cannot rewrite its source checkpoint"
        );
    }
}

#[cfg(test)]
mod text_search_tests {
    use super::*;

    // A thread's pull request records are read oldest first after an offset,
    // from this conversation's visible item events only; replies, other
    // conversations, hidden rows and unrelated items are skipped.
    #[tokio::test]
    async fn pull_request_records_read_visible_events_after_an_offset() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let event = |command: &str| {
            serde_json::json!({"event":{"data":{"detail":
                serde_json::json!({"item":{"type":"commandExecution","command":command}}).to_string()}}})
            .to_string()
        };
        for (conversation, source, id, body) in [
            ("chat", "event", "e1", event("gh pr create --fill")),
            ("chat", "event", "e2", event("ls")),
            (
                "chat",
                "assistant",
                "a1",
                "I will git push and gh pr create".to_owned(),
            ),
            ("other", "event", "e3", event("git push -u origin x")),
            ("chat", "event", "e4", event("git push -u origin x")),
            ("chat", "event", "e5", event("gh pr new")),
        ] {
            sqlx::query("INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json) VALUES (?,?,?,0,?)")
                .bind(conversation).bind(source).bind(id).bind(body)
                .execute(&store.pool).await.unwrap();
        }
        sqlx::query("UPDATE history_entries SET hidden_by_reset=1 WHERE source_id='e5'")
            .execute(&store.pool)
            .await
            .unwrap();
        let (revision, sequences) = store
            .conversation_pull_request_records("chat", 0, 10)
            .await
            .unwrap();
        assert_eq!(revision, 0);
        let rows = store
            .history_event_details("chat", &sequences)
            .await
            .unwrap();
        let commands: Vec<_> = rows
            .iter()
            .map(|(_, detail)| detail.contains("pr create"))
            .collect();
        assert_eq!(commands, [true, false], "{rows:?}");
        assert_eq!(
            rows.iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            sequences
        );
        let (_, rest) = store
            .conversation_pull_request_records("chat", sequences[0], 10)
            .await
            .unwrap();
        assert_eq!(rest, sequences[1..]);
        let (_, first) = store
            .conversation_pull_request_records("chat", 0, 1)
            .await
            .unwrap();
        assert_eq!(first, sequences[..1]);
        // Another conversation's row is not read through this one.
        let (_, other) = store
            .conversation_pull_request_records("other", 0, 10)
            .await
            .unwrap();
        assert!(store
            .history_event_details("chat", &other)
            .await
            .unwrap()
            .is_empty());
    }
}
