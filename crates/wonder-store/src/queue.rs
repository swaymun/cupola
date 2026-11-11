//! Revision-checked edits share SQLite's write boundary with dispatch claims.
use super::*;

#[derive(Debug)]
pub struct QueueItem {
    pub id: String,
    pub client_message_id: String,
    pub body: String,
    pub revision: i64,
    pub attachment_ids: Vec<String>,
    pub codex_skills: Vec<CodexSkillSelection>,
    /// Waits behind other work in its conversation: a message being sent or
    /// answered, or an earlier waiting message. The first pending message with
    /// nothing ahead is being sent, not queued, even before dispatch claims it.
    pub behind_work: bool,
}

/// `messages m` waits behind other work in its conversation. The exact
/// complement of what dispatch may claim next (`pending_dispatch_messages`).
fn behind_work_sql() -> String {
    format!(
        "(EXISTS(SELECT 1 FROM messages active WHERE active.conversation_id=m.conversation_id AND active.id<>m.id AND active.state IN ({})) OR EXISTS(SELECT 1 FROM messages ahead JOIN dispatch_work aw ON aw.message_id=ahead.id WHERE ahead.conversation_id=m.conversation_id AND ahead.id<>m.id AND ahead.state='accepted_by_wonder' AND (ahead.queue_position,ahead.created_at,ahead.id)<(m.queue_position,m.created_at,m.id)))",
        turn_states_sql(TurnScope::InFlight)
    )
}

impl Store {
    /// Whether an accepted message waits behind other work (see `QueueItem::behind_work`).
    pub async fn message_behind_work(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(sql!(
            "SELECT {} FROM messages m WHERE m.id=? AND m.state='accepted_by_wonder'",
            behind_work_sql()
        ))
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .map(|value: Option<bool>| value.unwrap_or(false))
    }
    pub async fn pending_queue(&self, conversation: &str) -> Result<Vec<QueueItem>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(sql!("SELECT m.*, {} AS behind_work FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder' AND NOT EXISTS(SELECT 1 FROM message_sources s WHERE s.message_id=m.id AND s.kind='wake') ORDER BY m.queue_position,m.created_at,m.id", behind_work_sql()))
            .bind(conversation).fetch_all(&mut *tx).await?;
        let mut items = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let attachment_ids = sqlx::query_scalar(
                "SELECT file_id FROM message_attachments WHERE message_id=? ORDER BY file_id",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            items.push(QueueItem {
                id,
                client_message_id: row.get("client_message_id"),
                body: row.get("body"),
                revision: row.get("queue_revision"),
                attachment_ids,
                codex_skills: serde_json::from_str(row.get("codex_skills_json"))
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                behind_work: row.get("behind_work"),
            });
        }
        tx.commit().await?;
        Ok(items)
    }
    pub async fn edit_pending(
        &self,
        conversation: &str,
        id: &str,
        revision: i64,
        body: Option<(&str, &str)>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = if let Some((body, hash)) = body {
            sqlx::query("UPDATE messages SET body=?,body_sha256=?,queue_revision=queue_revision+1 WHERE id=? AND conversation_id=? AND queue_revision=? AND state='accepted_by_wonder' AND id IN (SELECT message_id FROM dispatch_work)")
                .bind(body).bind(hash).bind(id).bind(conversation).bind(revision).execute(&mut *tx).await?
        } else {
            sqlx::query("UPDATE messages SET state='interrupted',queue_revision=queue_revision+1 WHERE id=? AND conversation_id=? AND queue_revision=? AND state='accepted_by_wonder' AND id IN (SELECT message_id FROM dispatch_work)")
                .bind(id).bind(conversation).bind(revision).execute(&mut *tx).await?
        };
        if result.rows_affected() == 1 {
            let saved: String =
                sqlx::query_scalar("SELECT codex_skills_json FROM messages WHERE id=?")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            let mut skills: Vec<CodexSkillSelection> =
                serde_json::from_str(&saved).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            skills.retain(|skill| {
                body.is_some_and(|(text, _)| skill_token_present(text, &skill.name))
            });
            sqlx::query("UPDATE messages SET codex_skills_json=? WHERE id=?")
                .bind(
                    serde_json::to_string(&skills)
                        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if result.rows_affected() == 1 && body.is_none() {
            sqlx::query("DELETE FROM message_attachments WHERE message_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }
    pub async fn reorder_pending(
        &self,
        conversation: &str,
        order: &[(String, i64)],
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Acquire the write boundary before comparing the complete queue.
        sqlx::query("UPDATE messages SET queue_position=queue_position WHERE conversation_id=? AND state='accepted_by_wonder'").bind(conversation).execute(&mut *tx).await?;
        let rows = sqlx::query("SELECT m.id,m.queue_revision FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder'")
            .bind(conversation).fetch_all(&mut *tx).await?;
        let expected = order
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        if rows.len() != order.len()
            || expected.len() != order.len()
            || rows.iter().any(|r| {
                expected.get(&r.get::<String, _>("id")) != Some(&r.get::<i64, _>("queue_revision"))
            })
        {
            return Ok(false);
        }
        for (index, (id, _)) in order.iter().enumerate() {
            sqlx::query(
                "UPDATE messages SET queue_position=?,queue_revision=queue_revision+1 WHERE id=?",
            )
            .bind(index as i64 - order.len() as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
    pub async fn original_body_hash(&self, id: &str) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT original_body_sha256 FROM messages WHERE id=?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
    }
}

impl Store {
    pub async fn pending_guides(&self) -> Result<Vec<(StoredMessage, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.*,g.expected_turn_id FROM messages m JOIN guide_work g ON g.message_id=m.id WHERE m.state='accepted_by_wonder' ORDER BY m.created_at,m.id LIMIT 32").fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| (stored_message(r), r.get("expected_turn_id")))
            .collect())
    }
    pub async fn claim_guide(&self, id: &str, thread: &str) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE messages SET state='uncertain',codex_thread_id=? WHERE id=? AND state='accepted_by_wonder' AND id IN (SELECT message_id FROM guide_work)").bind(thread).bind(id).execute(&self.pool).await?.rows_affected()==1)
    }
    pub async fn queue_to_guide(
        &self,
        conversation: &str,
        id: &str,
        revision: i64,
        turn: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query(sql!("UPDATE messages SET queue_revision=queue_revision+1 WHERE id=? AND conversation_id=? AND queue_revision=? AND state='accepted_by_wonder' AND id IN (SELECT message_id FROM dispatch_work) AND EXISTS(SELECT 1 FROM messages active WHERE active.conversation_id=? AND active.codex_turn_id=? AND active.state IN ({}))", turn_states_sql(TurnScope::Running)))
            .bind(id).bind(conversation).bind(revision).bind(conversation).bind(turn).execute(&mut *tx).await?.rows_affected()==1;
        if changed {
            sqlx::query("DELETE FROM dispatch_work WHERE message_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO guide_work(message_id,expected_turn_id) VALUES (?,?)")
                .bind(id)
                .bind(turn)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// A Guide that could not reach the reply it was sent into (the reply
    /// ended, or the runtime refused it before reading it) waits at the end of
    /// its conversation's queue instead, as a message sent normally. Only a
    /// Guide not yet accepted by a runtime moves; an agent task's never does.
    pub async fn guide_to_queue(&self, id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE messages SET state='accepted_by_wonder',codex_thread_id=NULL,codex_turn_id=NULL,delivery_error=NULL,queue_revision=queue_revision+1,queue_position=(SELECT COALESCE(MAX(m.queue_position),-1)+1 FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=messages.conversation_id AND m.state='accepted_by_wonder') WHERE id=? AND state IN ('accepted_by_wonder','uncertain') AND id IN (SELECT message_id FROM guide_work) AND NOT EXISTS(SELECT 1 FROM subagent_ownership child WHERE child.conversation_id=messages.conversation_id)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if changed {
            sqlx::query("DELETE FROM guide_work WHERE message_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO dispatch_work(message_id) VALUES (?)")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// A send the runtime refused before reading it, because a reply was
    /// still running there, goes back to its place in the queue.
    pub async fn requeue_refused_send(&self, id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE messages SET state='accepted_by_wonder' WHERE id=? AND state='dispatching_to_codex' AND id IN (SELECT message_id FROM dispatch_work)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if changed {
            sqlx::query("UPDATE dispatch_attempts SET phase='released' WHERE id=(SELECT MAX(id) FROM dispatch_attempts WHERE message_id=?) AND phase IN ('claimed','submitting')")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> Store {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("phone", "Phone", "{}", "now")
            .await
            .unwrap();
        store
    }
    async fn message(store: &Store, client: &str) -> StoredMessage {
        message_at(store, client, client).await
    }
    async fn message_at(store: &Store, client: &str, created_at: &str) -> StoredMessage {
        match store
            .insert_dispatch_message(
                "phone",
                client,
                client,
                client,
                "chat",
                &[],
                created_at,
                true,
            )
            .await
            .unwrap()
        {
            MessageInsert::Inserted(m) => m,
            _ => panic!("new message"),
        }
    }
    // Owner (Send now, Oct 12): a Guide whose reply already ended failed
    // outright. It now waits in the queue behind what was already there, and
    // a send the runtime refused while its reply ran goes back to its place.
    #[tokio::test]
    async fn an_undeliverable_guide_joins_the_end_of_the_queue_and_a_refused_send_keeps_its_place()
    {
        let store = setup().await;
        let queued = message(&store, "queued").await;
        let MessageInsert::Inserted(guide) = store
            .insert_guide_message(
                "phone",
                "guide",
                "guide",
                "guide",
                "chat",
                &[],
                "later",
                "turn",
            )
            .await
            .unwrap()
        else {
            panic!("new guide")
        };
        assert!(store.claim_guide(&guide.id, "thread").await.unwrap());
        assert!(store.guide_to_queue(&guide.id).await.unwrap());
        assert!(
            !store.guide_to_queue(&guide.id).await.unwrap(),
            "moved once"
        );
        assert!(store.pending_guides().await.unwrap().is_empty());
        let pending = store.pending_queue("chat").await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![queued.id.as_str(), guide.id.as_str()]
        );
        let moved = store.message_by_id(&guide.id).await.unwrap().unwrap();
        assert_eq!(
            (moved.state.as_str(), moved.codex_thread_id),
            ("accepted_by_wonder", None)
        );

        assert!(store.claim_message_for_dispatch(&queued.id).await.unwrap());
        store
            .begin_dispatch_submission(&queued.id, "thread")
            .await
            .unwrap();
        assert!(store.requeue_refused_send(&queued.id).await.unwrap());
        assert_eq!(
            store.pending_queue("chat").await.unwrap()[0].id,
            queued.id,
            "the refused send keeps its place"
        );
        assert!(store.claim_message_for_dispatch(&queued.id).await.unwrap());
        store
            .begin_dispatch_submission(&queued.id, "thread")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn identical_created_at_durable_messages_preserve_insertion_and_claim_order() {
        let store = setup().await;
        let first = message_at(&store, "first", "same-time").await;
        let second = message_at(&store, "second", "same-time").await;

        let pending = store.pending_queue("chat").await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![first.id.as_str(), second.id.as_str()]
        );
        assert!(store.claim_message_for_dispatch(&first.id).await.unwrap());
        assert!(!store.claim_message_for_dispatch(&second.id).await.unwrap());
        store
            .update_message_delivery(&first.id, "completed", None, None)
            .await
            .unwrap();
        assert!(store.claim_message_for_dispatch(&second.id).await.unwrap());
    }

    #[tokio::test]
    async fn reorder_and_edit_cannot_race_claim_or_overwrite_a_revision() {
        let store = setup().await;
        let a = message(&store, "a").await;
        let b = message(&store, "b").await;
        assert!(store
            .reorder_pending("chat", &[(b.id.clone(), 1), (a.id.clone(), 1)])
            .await
            .unwrap());
        assert!(!store.claim_message_for_dispatch(&a.id).await.unwrap());
        assert!(!store
            .edit_pending("chat", &b.id, 1, Some(("stale", "stale")))
            .await
            .unwrap());
        let (edit, claim) = tokio::join!(
            store.edit_pending("chat", &b.id, 2, Some(("edited", "edited-hash"))),
            store.claim_message_for_dispatch(&b.id)
        );
        assert!(claim.unwrap());
        let body = store.message_by_id(&b.id).await.unwrap().unwrap().body;
        assert_eq!(body, if edit.unwrap() { "edited" } else { "b" });
        assert!(!store.edit_pending("chat", &b.id, 3, None).await.unwrap());
        assert_eq!(
            store.original_body_hash(&b.id).await.unwrap().as_deref(),
            Some("b")
        );
        assert!(!store.claim_message_for_dispatch(&a.id).await.unwrap());
        store
            .update_message_delivery(&b.id, "completed", None, None)
            .await
            .unwrap();
        assert!(store.claim_message_for_dispatch(&a.id).await.unwrap());
    }
    #[tokio::test]
    async fn queued_guide_moves_atomically_and_survives_recovery_without_becoming_send() {
        let store = setup().await;
        let a = message(&store, "a").await;
        store
            .update_message_delivery(&a.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let b = message(&store, "b").await;
        assert!(!store
            .queue_to_guide("chat", &b.id, 1, "stale-turn")
            .await
            .unwrap());
        assert!(store
            .queue_to_guide("chat", &b.id, 1, "turn")
            .await
            .unwrap());
        assert!(!store
            .queue_to_guide("chat", &b.id, 1, "turn")
            .await
            .unwrap());
        assert!(!store.is_durable_dispatch(&b.id).await.unwrap());
        store.recover_dispatch_claims().await.unwrap();
        let guides = store.pending_guides().await.unwrap();
        assert_eq!(guides.len(), 1);
        assert_eq!(guides[0].0.id, b.id);
        assert_eq!(guides[0].1, "turn");
        assert!(store.claim_guide(&b.id, "thread").await.unwrap());
        assert!(!store.claim_guide(&b.id, "thread").await.unwrap());
        store.recover_dispatch_claims().await.unwrap();
        assert!(store.pending_guides().await.unwrap().is_empty());
        assert_eq!(
            store.ambiguous_dispatch_messages().await.unwrap()[0].id,
            b.id
        );
        assert!(store.pending_dispatch_messages().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn guide_acceptance_registers_target_and_pending_cancel_releases_references() {
        let store = setup().await;
        let first = store
            .insert_guide_message("phone", "guide", "body", "hash", "chat", &[], "now", "turn")
            .await
            .unwrap();
        assert!(matches!(first, MessageInsert::Inserted(_)));
        assert!(matches!(
            store
                .insert_guide_message(
                    "phone",
                    "guide",
                    "body",
                    "hash",
                    "chat",
                    &[],
                    "now",
                    "other"
                )
                .await
                .unwrap(),
            MessageInsert::Conflict
        ));
        store.recover_dispatch_claims().await.unwrap();
        assert_eq!(store.pending_guides().await.unwrap().len(), 1);
        sqlx::query("INSERT INTO conversation_files(id,conversation_id,kind,name,state,created_at,updated_at) VALUES ('file','chat','attachment','notes','available','now','now')").execute(&store.pool).await.unwrap();
        let MessageInsert::Inserted(pending) = store
            .insert_dispatch_message(
                "phone",
                "cancel",
                "body",
                "hash",
                "chat",
                &["file".into()],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("pending")
        };
        assert_eq!(
            store.attachment_ids_for_message(&pending.id).await.unwrap(),
            vec!["file"]
        );
        assert!(store
            .edit_pending("chat", &pending.id, 1, None)
            .await
            .unwrap());
        assert!(!store.claim_message_for_dispatch(&pending.id).await.unwrap());
        assert!(store.pending_queue("chat").await.unwrap().is_empty());
        assert!(store
            .attachment_ids_for_message(&pending.id)
            .await
            .unwrap()
            .is_empty());
    }
    // A message is listed as queued only while it waits behind other work;
    // the one dispatch may claim next is being sent. The two predicates are
    // exact complements, so a direct send never shows as queued first.
    #[tokio::test]
    async fn only_messages_behind_other_work_are_queued() {
        let store = setup().await;
        let waiting = |store: &Store| {
            let store = store.clone();
            async move {
                let items = store.pending_queue("chat").await.unwrap();
                let next: Vec<String> = store
                    .pending_dispatch_messages()
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|m| m.id)
                    .collect();
                for item in &items {
                    assert_eq!(item.behind_work, !next.contains(&item.id), "{}", item.body);
                    assert_eq!(
                        store.message_behind_work(&item.id).await.unwrap(),
                        item.behind_work
                    );
                }
                items
                    .into_iter()
                    .filter(|item| item.behind_work)
                    .map(|item| item.body)
                    .collect::<Vec<_>>()
            }
        };
        let first = message_at(&store, "first", "1").await;
        assert!(
            waiting(&store).await.is_empty(),
            "nothing ahead: being sent"
        );
        message_at(&store, "second", "2").await;
        assert_eq!(waiting(&store).await, ["second"]);
        assert!(store.claim_message_for_dispatch(&first.id).await.unwrap());
        assert_eq!(
            waiting(&store).await,
            ["second"],
            "behind the turn being sent"
        );
        store
            .update_message_delivery(&first.id, "streaming", Some("t"), Some("turn"))
            .await
            .unwrap();
        assert_eq!(waiting(&store).await, ["second"], "behind the running turn");
        store
            .update_message_delivery(&first.id, "interrupted", Some("t"), Some("turn"))
            .await
            .unwrap();
        assert!(
            waiting(&store).await.is_empty(),
            "after Stop it is sent at once"
        );
        assert!(!store.message_behind_work(&first.id).await.unwrap());
    }
}
