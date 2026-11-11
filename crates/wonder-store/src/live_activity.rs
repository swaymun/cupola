//! What a Live Activity needs from the store: the registrations to report this
//! Mac's chats to, per-chat routes and labels, and Wonder-owned work. The
//! ActivityKit tokens and the card's state live in the push Worker, which merges
//! every computer's rows into one card. Nothing here stores or returns conversation text.
use super::*;

/// A phone registration this Mac reports its chats to.
#[derive(Clone, Debug)]
pub struct LiveActivityTarget {
    pub device_id: String,
    pub registration_id: String,
    pub endpoint: String,
    pub sender_secret: String,
}

/// What the Mac knows right now about Wonder-owned work, for the aggregate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedActivity {
    /// Top-level conversations with a turn in progress (subagents count as
    /// their verified parent Project thread).
    pub running: Vec<String>,
    /// Conversations a pending approval belongs to; `""` for one not routed yet.
    pub approvals: Vec<String>,
    /// Conversations with an unanswered question.
    pub questions: Vec<String>,
}

impl Store {
    /// Registrations of devices that may still receive pushes: where this Mac
    /// reports its chats for the phone's one Live Activity.
    pub async fn live_activity_registrations(
        &self,
    ) -> Result<Vec<LiveActivityTarget>, sqlx::Error> {
        let rows = sqlx::query("SELECT r.device_id,r.id AS registration_id,r.endpoint,r.sender_secret FROM push_registrations r JOIN devices d ON d.id=r.device_id AND d.revoked_at IS NULL AND d.forgotten=0 WHERE r.revoked=0 ORDER BY r.rowid")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| LiveActivityTarget {
                device_id: row.get("device_id"),
                registration_id: row.get("registration_id"),
                endpoint: row.get("endpoint"),
                sender_secret: row.get("sender_secret"),
            })
            .collect())
    }

    /// A random route for this device to `conversation`, resolved by the
    /// existing route lookup, so the push carries no conversation ID.
    pub async fn live_activity_route(
        &self,
        device: &str,
        conversation: &str,
    ) -> Result<String, sqlx::Error> {
        let route = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO push_routes(id,device_id,conversation_id) VALUES(?,?,?)")
            .bind(&route)
            .bind(device)
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        Ok(route)
    }

    /// The device's route to `conversation`, reused while recent (routes are
    /// pruned after a week) so a row's route stays stable between pushes and an
    /// unchanged state is not re-sent.
    pub async fn live_activity_thread_route(
        &self,
        device: &str,
        conversation: &str,
    ) -> Result<String, sqlx::Error> {
        let existing: Option<String> = sqlx::query_scalar("SELECT id FROM push_routes WHERE device_id=? AND conversation_id=? AND created_at>unixepoch()-518400 ORDER BY created_at DESC,rowid DESC LIMIT 1")
            .bind(device)
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await?;
        match existing {
            Some(route) => Ok(route),
            None => self.live_activity_route(device, conversation).await,
        }
    }

    /// A Project thread's name and its Project's name, for the Live Activity's
    /// per-thread rows (the owner allows chat titles in the push).
    pub async fn live_activity_thread_label(
        &self,
        conversation: &str,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT c.title,p.name FROM project_conversations c JOIN projects p ON p.id=c.project_id WHERE c.conversation_id=?")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await
    }

    /// Wonder-owned work: the same message states that make a sidebar row
    /// read as working, pending approvals and unanswered questions.
    pub async fn owned_activity(&self, now_ms: i64) -> Result<OwnedActivity, sqlx::Error> {
        let conversations: Vec<String> = sqlx::query_scalar(sql!("SELECT DISTINCT m.conversation_id FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE {} ORDER BY m.conversation_id", crate::working_message_sql("m")))
            .fetch_all(&self.pool)
            .await?;
        let mut running = Vec::new();
        for conversation in conversations {
            let root: Option<String> = sqlx::query_scalar(questions::VERIFIED_SUBAGENT_ROOT_SQL)
                .bind(&conversation)
                .fetch_optional(&self.pool)
                .await?;
            let owned: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM subagent_ownership WHERE conversation_id=?)",
            )
            .bind(&conversation)
            .fetch_one(&self.pool)
            .await?;
            // An unverified child task is not counted on its own.
            let top = match root {
                Some(root) => root,
                None if owned => continue,
                None => conversation,
            };
            if !running.contains(&top) {
                running.push(top);
            }
        }
        let pending: Vec<(String, String)> =
            sqlx::query_as("SELECT thread_id,turn_id FROM approvals WHERE state='pending'")
                .fetch_all(&self.pool)
                .await?;
        let mut approvals = Vec::new();
        for (thread, turn) in pending {
            approvals.push(
                self.approval_push_conversation(&thread, &turn)
                    .await?
                    .unwrap_or_default(),
            );
        }
        let questions: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT conversation_id FROM async_questions WHERE state='pending' AND expires_at_ms>?",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(OwnedActivity {
            running,
            approvals,
            questions,
        })
    }

    /// The conversation and outcome of the newest Wonder-owned turn on this Mac: the delivery state
    /// its final event published (the message row itself reads `completed`
    /// for any turn the harness finished, whatever the turn's outcome).
    pub async fn newest_owned_outcome(&self) -> Result<Option<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT COALESCE(json_extract(payload_json,'$.conversationId'),''),json_extract(payload_json,'$.event.data.state') FROM events WHERE json_extract(payload_json,'$.event.type')='message_state' ORDER BY rowid DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
    }

    /// How a conversation's newest Wonder-owned turn ended, from the same
    /// event (the event ledger is indexed by conversation).
    pub async fn latest_owned_outcome(
        &self,
        conversation: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT json_extract(payload_json,'$.event.data.state') FROM events WHERE json_extract(payload_json,'$.conversationId')=? AND json_extract(payload_json,'$.event.type')='message_state' ORDER BY rowid DESC LIMIT 1")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_go_to_active_registrations_only() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        for id in ["phone-a", "phone-b"] {
            store
                .upsert_owner_device(id, id, "{}", "now")
                .await
                .unwrap();
        }
        store
            .register_push("phone-a", "reg-a", "https://push.test", "secret-a")
            .await
            .unwrap();
        store
            .register_push("phone-b", "reg-b", "https://push.test", "secret-b")
            .await
            .unwrap();
        let ids = |targets: Vec<LiveActivityTarget>| -> Vec<String> {
            targets.into_iter().map(|t| t.registration_id).collect()
        };
        assert_eq!(
            ids(store.live_activity_registrations().await.unwrap()),
            ["reg-a", "reg-b"]
        );
        // Turning notifications off, or revoking the device, ends reports to it.
        store.revoke_push("phone-a").await.unwrap();
        store.revoke_owner_device("phone-b", "now").await.unwrap();
        assert!(store
            .live_activity_registrations()
            .await
            .unwrap()
            .is_empty());
    }
}
