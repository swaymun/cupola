//! Durable state for agents that message and delegate to other Project threads.
use super::*;

/// Where a message came from when a thread, not the owner, wrote it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSource {
    /// `thread` (sent by another thread), `delegation` (a delegated task's
    /// prompt), `wake` (Wonder reporting finished delegated tasks) or
    /// `automation` (a scheduled task's prompt; derived, never stored).
    pub kind: String,
    pub source_conversation_id: Option<String>,
    pub source_title: String,
}

/// The source stored in the same transaction as the message it labels.
#[derive(Clone, Copy, Debug)]
pub struct NewMessageSource<'a> {
    pub kind: &'a str,
    pub conversation: Option<&'a str>,
    pub title: &'a str,
    /// For `wake`: the finished children this message reports.
    pub wake_children: &'a [String],
    /// For `wake`: the parent the children belong to.
    pub wake_parent: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadDelegation {
    pub child_conversation_id: String,
    pub parent_conversation_id: String,
    pub client_request_id: String,
    pub device_id: String,
    pub created_at: String,
}

/// A delegated task's recorded result; see migration 0107.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationResult {
    /// `completed`, `stopped` or `failed`.
    pub outcome: String,
    /// The child's final reply, when its history could be read.
    pub text: Option<String>,
    pub truncated: bool,
    pub at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeChild {
    pub conversation_id: String,
    pub title: String,
    /// `completed`, `stopped` or `failed`, from the child's newest message.
    pub outcome: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeCandidate {
    pub parent_conversation_id: String,
    pub device_id: String,
    pub children: Vec<WakeChild>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ThreadRunState {
    pub working: bool,
    pub queued: i64,
    pub waiting: bool,
    /// The running turn waits for the owner to answer a question.
    pub question: bool,
}

/// A delegated child's link to its parent, written in the transaction that
/// creates the child so the parent's limit holds under parallel requests.
pub(crate) struct DelegationReservation<'a> {
    pub parent: &'a str,
    pub client_request_id: &'a str,
    pub device_id: &'a str,
    pub now: &'a str,
    pub max_children: i64,
}

impl DelegationReservation<'_> {
    async fn existing(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT child_conversation_id FROM thread_delegations WHERE parent_conversation_id=? AND client_request_id=?")
            .bind(self.parent).bind(self.client_request_id).fetch_optional(&mut **tx).await
    }

    /// A retried request always has room; a new one only below the limit.
    pub(crate) async fn has_room(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<bool, sqlx::Error> {
        if self.existing(tx).await?.is_some() {
            return Ok(true);
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM thread_delegations WHERE parent_conversation_id=?",
        )
        .bind(self.parent)
        .fetch_one(&mut **tx)
        .await?;
        Ok(count < self.max_children)
    }

    /// Records `child` under the parent unless this request already did.
    pub(crate) async fn reserve(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        child: &str,
    ) -> Result<bool, sqlx::Error> {
        match self.existing(tx).await? {
            Some(existing) if existing == child => return Ok(true),
            Some(_) => return Err(sqlx::Error::Protocol("delegation_request_reused".into())),
            None => {}
        }
        if !self.has_room(tx).await? {
            return Ok(false);
        }
        sqlx::query("INSERT INTO thread_delegations(child_conversation_id,parent_conversation_id,client_request_id,device_id,created_at) VALUES(?,?,?,?,?)")
            .bind(child).bind(self.parent).bind(self.client_request_id).bind(self.device_id).bind(self.now)
            .execute(&mut **tx).await?;
        Ok(true)
    }
}

fn delegation(row: &sqlx::sqlite::SqliteRow) -> ThreadDelegation {
    ThreadDelegation {
        child_conversation_id: row.get("child_conversation_id"),
        parent_conversation_id: row.get("parent_conversation_id"),
        client_request_id: row.get("client_request_id"),
        device_id: row.get("device_id"),
        created_at: row.get("created_at"),
    }
}

impl Store {
    /// Inserts the source row and, for a wake-up, claims its children. Runs
    /// inside the message's own transaction so neither can exist alone.
    pub(crate) async fn insert_message_source(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        message: &str,
        source: &NewMessageSource<'_>,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query("INSERT INTO message_sources(message_id,kind,source_conversation_id,source_title) VALUES(?,?,?,?)")
            .bind(message).bind(source.kind).bind(source.conversation).bind(source.title)
            .execute(&mut **transaction).await?;
        if source.kind != "wake" {
            return Ok(true);
        }
        let mut claimed = 0;
        for child in source.wake_children {
            claimed += sqlx::query("UPDATE thread_delegations SET wake_message_id=? WHERE child_conversation_id=? AND parent_conversation_id=? AND wake_message_id IS NULL AND observed_at IS NULL")
                .bind(message).bind(child).bind(source.wake_parent)
                .execute(&mut **transaction).await?.rows_affected();
        }
        Ok(claimed > 0)
    }

    /// Queues a message written by an agent, labelled with its source. A
    /// repeated `client_message_id` returns the original message.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        conversation_id: &str,
        now: &str,
        source: &NewMessageSource<'_>,
    ) -> Result<MessageInsert, sqlx::Error> {
        let hash = hex_sha256(body);
        self.insert_work_message(
            device_id,
            client_message_id,
            body,
            &hash,
            conversation_id,
            &[],
            now,
            true,
            None,
            Some(source),
            &[],
        )
        .await
    }

    /// Steers a running turn with a message written by an agent.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_guide(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        conversation_id: &str,
        now: &str,
        turn: &str,
        source: &NewMessageSource<'_>,
    ) -> Result<MessageInsert, sqlx::Error> {
        let hash = hex_sha256(body);
        self.insert_work_message(
            device_id,
            client_message_id,
            body,
            &hash,
            conversation_id,
            &[],
            now,
            false,
            Some(turn),
            Some(source),
            &[],
        )
        .await
    }

    pub async fn message_source(
        &self,
        message: &str,
    ) -> Result<Option<MessageSource>, sqlx::Error> {
        Ok(sqlx::query("SELECT kind,source_conversation_id,source_title FROM message_sources WHERE message_id=?")
            .bind(message).fetch_optional(&self.pool).await?
            .map(|row| MessageSource {
                kind: row.get("kind"),
                source_conversation_id: row.get("source_conversation_id"),
                source_title: row.get("source_title"),
            }))
    }

    pub async fn message_sources(
        &self,
        conversation: &str,
    ) -> Result<HashMap<String, MessageSource>, sqlx::Error> {
        let rows = sqlx::query("SELECT s.message_id,s.kind,s.source_conversation_id,s.source_title FROM message_sources s JOIN messages m ON m.id=s.message_id WHERE m.conversation_id=?")
            .bind(conversation).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get("message_id"),
                    MessageSource {
                        kind: row.get("kind"),
                        source_conversation_id: row.get("source_conversation_id"),
                        source_title: row.get("source_title"),
                    },
                )
            })
            .collect())
    }

    /// The same labels keyed by the id the client chose, which provider
    /// history repeats on each of its user messages.
    pub async fn message_source_labels(
        &self,
        conversation: &str,
    ) -> Result<HashMap<String, MessageSource>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.client_message_id,s.kind,s.source_conversation_id,s.source_title FROM message_sources s JOIN messages m ON m.id=s.message_id WHERE m.conversation_id=?")
            .bind(conversation).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get("client_message_id"),
                    MessageSource {
                        kind: row.get("kind"),
                        source_conversation_id: row.get("source_conversation_id"),
                        source_title: row.get("source_title"),
                    },
                )
            })
            .collect())
    }

    /// Text of messages waiting behind a conversation's running work, oldest
    /// first, with who wrote each when an agent did.
    pub async fn queued_message_bodies(
        &self,
        conversation: &str,
    ) -> Result<Vec<(String, Option<MessageSource>)>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.body,s.kind,s.source_conversation_id,s.source_title FROM messages m JOIN dispatch_work w ON w.message_id=m.id LEFT JOIN message_sources s ON s.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder' AND COALESCE(s.kind,'')<>'wake' ORDER BY m.queue_position,m.created_at,m.id")
            .bind(conversation).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|row| {
                let kind: Option<String> = row.get("kind");
                (
                    row.get("body"),
                    kind.map(|kind| MessageSource {
                        kind,
                        source_conversation_id: row.get("source_conversation_id"),
                        source_title: row.get("source_title"),
                    }),
                )
            })
            .collect())
    }

    /// Creates a delegated child and records its parent in one transaction.
    /// A retry of the same request returns the same child; None means the
    /// parent already has `max_children` children and nothing was created.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_delegated_conversation(
        &self,
        insert: ProjectConversationInsert<'_>,
        request_digest: &str,
        parent: &str,
        client_request_id: &str,
        device_id: &str,
        max_children: i64,
    ) -> Result<Option<ProjectConversationCreate>, sqlx::Error> {
        let now = insert.now;
        let reservation = DelegationReservation {
            parent,
            client_request_id,
            device_id,
            now,
            max_children,
        };
        self.create_project_conversation_inner(insert, Some(request_digest), Some(&reservation))
            .await
    }

    /// Removes delegated children a crash left without their task: created
    /// before `cutoff`, never given a message and never started. Delegation
    /// inserts the task right after the child, so only an interrupted request
    /// leaves one; a retry of that request creates it again.
    pub async fn remove_abandoned_delegations(&self, cutoff: &str) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query("DELETE FROM conversations WHERE id IN (SELECT d.child_conversation_id FROM thread_delegations d JOIN project_conversations p ON p.conversation_id=d.child_conversation_id WHERE d.created_at<? AND p.native_session_id IS NULL AND NOT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id=d.child_conversation_id) AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.conversation_id=d.child_conversation_id))")
            .bind(cutoff).execute(&self.pool).await?.rows_affected())
    }

    /// Records the parent of a delegated child; a repeated request changes nothing.
    pub async fn record_thread_delegation(
        &self,
        child: &str,
        parent: &str,
        client_request_id: &str,
        device_id: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO thread_delegations(child_conversation_id,parent_conversation_id,client_request_id,device_id,created_at) VALUES(?,?,?,?,?)")
            .bind(child).bind(parent).bind(client_request_id).bind(device_id).bind(now)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn thread_delegation(
        &self,
        child: &str,
    ) -> Result<Option<ThreadDelegation>, sqlx::Error> {
        Ok(
            sqlx::query("SELECT * FROM thread_delegations WHERE child_conversation_id=?")
                .bind(child)
                .fetch_optional(&self.pool)
                .await?
                .as_ref()
                .map(delegation),
        )
    }

    /// The threads `parent` delegated, oldest first.
    pub async fn thread_delegation_children(
        &self,
        parent: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT child_conversation_id FROM thread_delegations WHERE parent_conversation_id=? ORDER BY rowid")
            .bind(parent)
            .fetch_all(&self.pool)
            .await
    }

    pub async fn thread_delegation_by_request(
        &self,
        parent: &str,
        client_request_id: &str,
    ) -> Result<Option<ThreadDelegation>, sqlx::Error> {
        Ok(sqlx::query(
            "SELECT * FROM thread_delegations WHERE parent_conversation_id=? AND client_request_id=?",
        )
        .bind(parent)
        .bind(client_request_id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(delegation))
    }

    /// How many delegations sit above this thread.
    pub async fn thread_delegation_depth(&self, conversation: &str) -> Result<i64, sqlx::Error> {
        let mut depth = 0;
        let mut current = conversation.to_owned();
        while depth < 16 {
            let parent: Option<String> = sqlx::query_scalar("SELECT parent_conversation_id FROM thread_delegations WHERE child_conversation_id=?")
                .bind(&current).fetch_optional(&self.pool).await?;
            match parent {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => break,
            }
        }
        Ok(depth)
    }

    pub async fn thread_delegation_count(&self, parent: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM thread_delegations WHERE parent_conversation_id=?")
            .bind(parent)
            .fetch_one(&self.pool)
            .await
    }

    /// Records a delegated task's result once; the first one stays.
    pub async fn record_delegation_result(
        &self,
        child: &str,
        result: &DelegationResult,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE thread_delegations SET result_outcome=?,result_text=?,result_truncated=?,result_at=? WHERE child_conversation_id=? AND result_at IS NULL")
            .bind(&result.outcome).bind(&result.text).bind(result.truncated).bind(&result.at).bind(child)
            .execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn delegation_result(
        &self,
        child: &str,
    ) -> Result<Option<DelegationResult>, sqlx::Error> {
        Ok(sqlx::query("SELECT result_outcome,result_text,result_truncated,result_at FROM thread_delegations WHERE child_conversation_id=? AND result_at IS NOT NULL")
            .bind(child).fetch_optional(&self.pool).await?
            .map(|row| DelegationResult {
                outcome: row.get("result_outcome"),
                text: row.get("result_text"),
                truncated: row.get("result_truncated"),
                at: row.get("result_at"),
            }))
    }

    /// New work in a delegated child must be reported to its parent again.
    pub async fn rearm_thread_delegation(&self, child: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE thread_delegations SET wake_message_id=NULL,observed_at=NULL WHERE child_conversation_id=?")
            .bind(child).execute(&self.pool).await?;
        Ok(())
    }

    /// The parent read the child's result itself; no wake-up is needed.
    pub async fn observe_thread_delegation(
        &self,
        parent: &str,
        child: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE thread_delegations SET observed_at=? WHERE child_conversation_id=? AND parent_conversation_id=? AND wake_message_id IS NULL AND observed_at IS NULL")
            .bind(now).bind(child).bind(parent).execute(&self.pool).await?;
        Ok(())
    }

    /// Parents that are idle and have finished, unreported delegated children,
    /// with those children. A parent that is running or has queued work is
    /// left alone: its turn is already going to see the result.
    pub async fn wake_candidates(&self) -> Result<Vec<WakeCandidate>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT d.parent_conversation_id,d.child_conversation_id,d.device_id,c.title,
               (SELECT state FROM messages WHERE conversation_id=d.child_conversation_id ORDER BY created_at DESC,rowid DESC LIMIT 1) AS latest
             FROM thread_delegations d JOIN project_conversations c ON c.conversation_id=d.child_conversation_id
             WHERE d.wake_message_id IS NULL AND d.observed_at IS NULL
               AND EXISTS(SELECT 1 FROM messages WHERE conversation_id=d.child_conversation_id)
               AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id=d.child_conversation_id AND state IN ('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming'))
               AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id=d.parent_conversation_id AND state IN ('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming'))
             ORDER BY d.parent_conversation_id,d.created_at,d.child_conversation_id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<WakeCandidate> = Vec::new();
        for row in rows {
            let parent: String = row.get("parent_conversation_id");
            let latest: Option<String> = row.get("latest");
            let child = WakeChild {
                conversation_id: row.get("child_conversation_id"),
                title: row.get("title"),
                outcome: match latest.as_deref() {
                    Some("completed") => "completed",
                    Some("interrupted") => "stopped",
                    _ => "failed",
                }
                .into(),
            };
            match out.last_mut() {
                Some(last) if last.parent_conversation_id == parent => last.children.push(child),
                _ => out.push(WakeCandidate {
                    parent_conversation_id: parent,
                    device_id: row.get("device_id"),
                    children: vec![child],
                }),
            }
        }
        Ok(out)
    }

    pub async fn thread_run_state(
        &self,
        conversation: &str,
    ) -> Result<ThreadRunState, sqlx::Error> {
        let working: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ('dispatching_to_codex','accepted_by_codex','streaming'))")
        .bind(conversation)
        .fetch_one(&self.pool)
        .await?;
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder'")
            .bind(conversation).fetch_one(&self.pool).await?;
        let (waiting, question): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(a.method='item/tool/requestUserInput'),0) FROM approvals a JOIN messages m ON m.codex_thread_id=a.thread_id AND m.codex_turn_id=a.turn_id WHERE m.conversation_id=? AND a.state IN ('pending','resolving')")
            .bind(conversation).fetch_one(&self.pool).await?;
        Ok(ThreadRunState {
            working: working != 0,
            queued,
            waiting: working != 0 && waiting != 0,
            question: working != 0 && question != 0,
        })
    }

    /// State of the conversation's newest message; `None` before any was sent.
    pub async fn latest_message_state(
        &self,
        conversation: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT state FROM messages WHERE conversation_id=? ORDER BY created_at DESC,rowid DESC LIMIT 1")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await
    }

    /// Ids of messages that are queued or running in this conversation.
    pub async fn open_message_ids(&self, conversation: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT id FROM messages WHERE conversation_id=? AND state IN ('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming') ORDER BY created_at,rowid")
        .bind(conversation)
        .fetch_all(&self.pool)
        .await
    }

    /// The turn that is running now, if Wonder started it.
    pub async fn running_message(
        &self,
        conversation: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        Ok(sqlx::query("SELECT * FROM messages WHERE conversation_id=? AND state IN ('accepted_by_codex','streaming') AND codex_turn_id IS NOT NULL ORDER BY created_at DESC,rowid DESC LIMIT 1")
            .bind(conversation).fetch_optional(&self.pool).await?
            .as_ref().map(stored_message))
    }

    /// Whether a dispatch has claimed a message of this conversation and not
    /// yet stored the turn it started (or put it back).
    pub async fn starting_message(&self, conversation: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state='dispatching_to_codex')")
            .bind(conversation)
            .fetch_one(&self.pool)
            .await
    }

    /// The newest message of a conversation, whatever its state.
    pub async fn latest_message(
        &self,
        conversation: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        Ok(sqlx::query("SELECT * FROM messages WHERE conversation_id=? ORDER BY created_at DESC,rowid DESC LIMIT 1")
            .bind(conversation).fetch_optional(&self.pool).await?
            .as_ref().map(stored_message))
    }
}

fn hex_sha256(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

impl Store {
    /// The caller compares an existing input hash before reusing its durable response.
    pub async fn tool_call(
        &self,
        key: &str,
    ) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
        Ok(
            sqlx::query("SELECT input_hash,response_json FROM tool_calls WHERE call_key=?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| (r.get("input_hash"), r.get("response_json"))),
        )
    }
    /// Claims `key` for one action in a single statement. None means this
    /// caller inserted the row and alone runs the action; otherwise it gets
    /// the row an earlier or concurrent attempt claimed (input hash, saved
    /// response).
    pub async fn claim_tool_call(
        &self,
        key: &str,
        hash: &str,
        message: &str,
        runtime: &str,
        tool: &str,
        now: &str,
    ) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
        let inserted = sqlx::query("INSERT OR IGNORE INTO tool_calls(call_key,input_hash,message_id,runtime_id,tool_name,created_at) VALUES(?,?,?,?,?,?)")
            .bind(key).bind(hash).bind(message).bind(runtime).bind(tool).bind(now)
            .execute(&self.pool).await?.rows_affected();
        if inserted == 1 {
            return Ok(None);
        }
        self.tool_call(key).await
    }

    /// A process that died while stopping a thread cannot tell whether the
    /// interruption reached the provider. Preserve that uncertainty instead
    /// of repeating an action that could stop newer work. The daemon has one
    /// process owner per store; `owner` changes on every process start.
    pub async fn recover_abandoned_stop_call(
        &self,
        key: &str,
        hash: &str,
        owner: &str,
        response: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE tool_calls SET response_json=? WHERE call_key=? AND input_hash=? AND tool_name='wonder_thread_stop' AND (runtime_id='agent-stop' OR runtime_id GLOB 'agent-stop:*') AND runtime_id<>? AND response_json IS NULL")
            .bind(response).bind(key).bind(hash).bind(owner)
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Reserves one runtime tool call, allowing at most `limit` per message.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_tool_call(
        &self,
        key: &str,
        hash: &str,
        message: &str,
        runtime: &str,
        tool: &str,
        now: &str,
        limit: i64,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            sqlx::query_scalar::<_, String>("SELECT input_hash FROM tool_calls WHERE call_key=?")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?
        {
            return Ok(existing == hash);
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_calls WHERE message_id=?")
            .bind(message)
            .fetch_one(&mut *tx)
            .await?;
        if count >= limit {
            return Ok(false);
        }
        sqlx::query("INSERT INTO tool_calls(call_key,input_hash,message_id,runtime_id,tool_name,created_at) VALUES(?,?,?,?,?,?)").bind(key).bind(hash).bind(message).bind(runtime).bind(tool).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn complete_tool_call(
        &self,
        key: &str,
        hash: &str,
        response: &str,
    ) -> Result<(), sqlx::Error> {
        let result=sqlx::query("UPDATE tool_calls SET response_json=? WHERE call_key=? AND input_hash=? AND response_json IS NULL").bind(response).bind(key).bind(hash).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }
    pub async fn dispatch_context(&self, message: &str) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT context_json FROM dispatch_attempts WHERE message_id=? ORDER BY id DESC LIMIT 1").bind(message).fetch_optional(&self.pool).await.map(Option::flatten)
    }
}
