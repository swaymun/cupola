//! SQLite persistence for Wonder-owned metadata and the durable event ledger.

/// A query built from `DeliveryState` lists (`turn_states_sql`,
/// `states_before_sql`): the only interpolated text is generated from the enum.
macro_rules! sql {
    ($($format:tt)*) => {
        sqlx::AssertSqlSafe(format!($($format)*))
    };
}

mod codex_skills;
pub use codex_skills::{skill_token_present, CodexSkillSelection};
mod agent_preferences;
mod approvals;
mod media_sources;
mod provider_runtime_config;
mod pull_request_links;
mod rewind;
mod t3_code_threads;
mod voice;
pub use media_sources::{MediaSource, MediaSourceLocation};
pub use rewind::{
    is_rewind_pending, ProjectRewind, ProviderSetting, RewindOperation, REWIND_PENDING,
};
pub use t3_code_threads::{T3CodeSend, T3EchoCache};
pub use voice::{
    VoicePanel, VoiceSettings, VoiceTranscriptEntry, VoiceTranscriptSummary, VoiceUsageEvent,
    VOICE_SESSION_LIMIT_DEFAULT_MINUTES, VOICE_SESSION_LIMIT_MAX_MINUTES,
};
mod agent_settings;
mod host_name;
pub mod mcp;
pub use host_name::HOST_NAME_MAX_CHARS;
mod wonder_skills;
pub use agent_preferences::AgentDefaultPreference;
pub use agent_settings::{AgentFamilySettings, ChatDetail, INSTRUCTION_MAX_CHARS};
mod automations;
pub use automations::ThreadAutomationRun;
mod push;
pub use push::{
    notification_question, AgentNotification, AgentNotificationOutcome, PushDelivery, PushOpen,
    PushPreview, PushRevocation, AGENT_NOTIFY_LIMIT, AGENT_NOTIFY_WINDOW_SECONDS,
};
mod live_activity;
pub use live_activity::{LiveActivityTarget, OwnedActivity};
mod questions;
pub use questions::AsyncQuestion;
mod provider;
pub use provider::{
    AgentTasks, ApprovalEnforcement, ArchiveStyle, BackgroundWork, BrowserUse, DefaultSpeed,
    FileMention, HistorySearch, InstructionDelivery, NativeSessionKey, PermissionProfile,
    PlanStyle, ProviderCapabilities, ProviderDescriptor, TerminalStatus, WonderSkillDelivery,
    CAPABILITIES_VERSION,
};
mod runtime_bindings;
pub use runtime_bindings::{AgentFamily, RuntimeBinding};
mod update_handoff;
pub use update_handoff::UpdateHandoff;
mod provider_switch;
pub use provider_switch::{
    HandoffDelivery, MessageTarget, PendingHandoff, ProjectRuntimeSwitch, RuntimeHistoryEntry,
};
mod continuations;
pub use continuations::ContinuationSettings;
mod deleted_chats;
mod projects;
mod turn_stops;
pub use deleted_chats::DeleteProjectChat;
pub use projects::{
    AutoArchiveRule, NativeConversationOwner, ProjectConversationCreate, ProjectConversationInsert,
    ProjectConversationPatch, ProjectCreate, ProjectLastModel, ProjectRootInput, StoredProject,
    StoredProjectConversation, StoredProjectRoot, EXECUTION_SCOPE_PROJECTS, MAX_PROJECT_ROOTS,
};

use std::{cmp::Ordering, collections::HashMap, time::Duration};

use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use wonder_api::{
    states_before_sql, turn_states_sql, DeliveryState, HostEventEnvelope, TurnScope, WonderEvent,
};

pub const ARTIFACT_ANNOTATION_MIME: &str = "application/vnd.wonder.artifact-annotation+json";

mod computer_sessions;
pub use computer_sessions::{
    ComputerControlLeaseCreate, ComputerLeaseAcquireResult, StoredComputerControlLease,
};
pub use computer_sessions::{
    ComputerSessionCreate, ComputerSessionState, StoredComputerSession, HOST_VIEW_CONVERSATION_ID,
};
mod thread_tools;
pub use thread_tools::{
    DelegationResult, MessageSource, NewMessageSource, ThreadDelegation, ThreadRunState,
    WakeCandidate, WakeChild,
};
mod queue;
pub use queue::QueueItem;
mod goals;
pub use goals::GoalTimeLimit;
mod history;
mod search;
pub use search::{SearchCursor, SearchPage};
mod retention;
pub use history::HistoryCursor;
pub use retention::{PeriodicCleanup, ReplayStorageUsage, PERIODIC_CLEANUP_INTERVAL};
mod sync;
pub use sync::{CommittedConversationSnapshot, ReplayBatch};

#[cfg(test)]
mod safety_tests;

/// Host-owned per-chat policy. Missing preferences preserve existing behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatNotificationLevel {
    #[default]
    Default,
    Follow,
    Mute,
}

impl ChatNotificationLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Follow => "follow",
            Self::Mute => "mute",
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceResetResult {
    pub revoked_device_ids: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MessageInsert {
    Inserted(StoredMessage),
    Existing(StoredMessage),
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredMessage {
    pub id: String,
    pub device_id: String,
    pub client_message_id: String,
    pub body: String,
    pub body_sha256: String,
    pub conversation_id: String,
    pub state: String,
    pub created_at: String,
    pub codex_thread_id: Option<String>,
    pub codex_turn_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredAssistantMessage {
    pub id: String,
    pub conversation_id: String,
    pub codex_thread_id: String,
    pub codex_turn_id: String,
    pub item_id: String,
    pub text: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPendingAppServerNotification {
    pub id: String,
    pub identity_key: Option<String>,
    pub method: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub notification_json: String,
    pub params_json: String,
    pub received_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSearchResult {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub snippet: Option<String>,
    pub conversation_id: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSubagentOwnership {
    pub conversation_id: String,
    pub parent_conversation_id: String,
    pub thread_id: String,
    pub parent_thread_id: String,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub agent_path: Option<String>,
    pub source_json: String,
    pub runtime_id: Option<String>,
    pub can_accept_direct_input: Option<bool>,
    pub status: String,
    pub is_archived: bool,
}

/// The FTS5 query for owner text: up to eight alphanumeric prefix terms,
/// all required. Shared by every searchable index so matching agrees.
pub fn search_match_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .take(8)
        .map(|term| format!("\"{term}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parsed_timestamp(value: &str) -> Option<i128> {
    let value = value.trim();
    if let Ok(milliseconds) = value.parse::<i128>() {
        return milliseconds.checked_mul(1_000_000);
    }
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|timestamp| timestamp.unix_timestamp_nanos())
}

fn compare_timestamps(left: &str, right: &str) -> Ordering {
    match (parsed_timestamp(left), parsed_timestamp(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        _ => left.cmp(right),
    }
}

/// How long an opener waits for another opener's schema upgrade.
/// Keep in step with the `busy_timeout` pragma in `migrate`.
const MIGRATION_WAIT: Duration = Duration::from_secs(120);

fn stored_expiration(value: i64, column: &str) -> Result<u64, sqlx::Error> {
    u64::try_from(value).map_err(|error| sqlx::Error::ColumnDecode {
        index: column.into(),
        source: Box::new(error),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredDevice {
    pub id: String,
    pub created_at: String,
    pub label: String,
    pub public_key_jwk: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
    pub session_expires_at_ms: Option<u64>,
    /// Hardware model the device reported when it paired.
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSession {
    pub token_hash: String,
    pub device_id: String,
    pub csrf_hash: String,
    pub expires_at_ms: u64,
}

/// Whose files a Project's Outputs index lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectOutputOrigin {
    /// What the threads' agents produced (tool media and visuals).
    Agent,
    /// What the owner attached, from the phone or the provider's transcript.
    Owner,
    Any,
}

/// The Outputs index's type filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectOutputType {
    All,
    /// HTML pages and native visuals.
    Pages,
    Images,
    /// Everything that is neither a page nor an image.
    Documents,
}

/// Where the next Outputs page starts: after this row in index order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectOutputCursor {
    pub pinned: bool,
    pub updated_at: String,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectOutputQuery<'a> {
    pub origin: ProjectOutputOrigin,
    pub file_type: ProjectOutputType,
    /// Case-insensitive part of the file name.
    pub name: Option<&'a str>,
    pub after: Option<ProjectOutputCursor>,
    pub limit: i64,
}

/// One file in a Project's Outputs index, with the thread that made it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProjectOutput {
    pub file: StoredConversationFile,
    pub conversation_title: String,
    /// The earlier output this one updates, while that record still exists.
    pub replaces_file_id: Option<String>,
    pub pinned: bool,
}

/// What `remove_unsent_upload` did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadRemoval {
    Removed(Box<StoredConversationFile>),
    /// A message already carries it.
    Referenced,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredConversationFile {
    pub id: String,
    pub conversation_id: String,
    pub kind: String,
    pub name: String,
    pub mime_type: Option<String>,
    pub byte_size: Option<i64>,
    pub sha256: Option<String>,
    pub relative_path: Option<String>,
    pub state: String,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub source_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope_type", rename_all = "snake_case")]
pub enum AutomationTarget {
    ProjectThread { scope_id: String },
}

impl AutomationTarget {
    pub fn scope_type(&self) -> &'static str {
        match self {
            Self::ProjectThread { .. } => "project_thread",
        }
    }
    pub fn scope_id(&self) -> &str {
        match self {
            Self::ProjectThread { scope_id } => scope_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredAutomation {
    pub id: String,
    #[serde(default)]
    pub revision: i64,
    pub name: String,
    pub kind: String,
    #[serde(flatten)]
    pub target: AutomationTarget,
    pub conversation_id: Option<String>,
    pub prompt: String,
    pub rrule: String,
    pub timezone: String,
    pub status: String,
    pub notification_policy: String,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredAutomationRun {
    pub id: String,
    pub automation_id: String,
    pub scheduled_for: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub message_id: Option<String>,
    pub conversation_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredApproval {
    pub approval_id: String,
    pub server_request_id: String,
    pub method: String,
    pub params_json: String,
    pub action_nonce: String,
    pub state: String,
    pub decision: Option<String>,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub resolution_idempotency_key: Option<String>,
    pub resolution_body_sha256: Option<String>,
    pub resolution_json: Option<String>,
}

/// SQL for "this message makes its conversation read as working" over
/// `messages <alias>`: Cupola-owned unfinished work (queued, in flight or
/// running) or an unconfirmed delivery, with or without a turn id, because
/// the host is still reconciling it. Uncertain is the same answer here and
/// in the provider-switch/rewind busy refusals (`TurnScope::Unsettled`);
/// the only difference is queued work, which waits behind a switch but
/// still reads as working.
pub(crate) fn working_message_sql(alias: &str) -> String {
    format!("{alias}.state IN ({})", turn_states_sql(TurnScope::Held))
}

impl Store {
    /// Resolve an approval from its exact persisted runtime turn or child ownership.
    /// Exact turn correlation remains scoped to a current Project binding.
    /// Child requests may arrive before their history; only a verified current
    /// descendant can provide the fallback. Runtime responses retain the exact
    /// request id, turn id, and runtime generation stored with the approval.
    pub async fn approval_conversation_id(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        if thread.is_empty() {
            return Ok(None);
        }
        let candidate = if let Some(message) = if turn.is_empty() {
            None
        } else {
            self.message_for_codex_thread_and_turn(thread, turn).await?
        } {
            Some(message.conversation_id)
        } else if let Some(conversation) = if turn.is_empty() {
            None
        } else {
            self.goal_conversation_for_turn(thread, turn).await?
        } {
            Some(conversation)
        } else {
            self.subagent_ownership_for_thread(thread)
                .await?
                .filter(|child| !child.is_archived)
                .map(|child| child.conversation_id)
        };
        let Some(conversation) = candidate else {
            return Ok(None);
        };
        if !self
            .verified_project_runtime_owner(&conversation, thread)
            .await?
        {
            return Ok(None);
        }
        Ok(Some(conversation))
    }

    /// Read the exact chat's preference (None means the chat does not exist).
    pub async fn chat_notification_level(
        &self,
        conversation: &str,
    ) -> Result<Option<ChatNotificationLevel>, sqlx::Error> {
        let level: Option<String> = sqlx::query_scalar(
            "SELECT notification_level FROM project_conversations WHERE conversation_id=?",
        )
        .bind(conversation)
        .fetch_optional(&self.pool)
        .await?;
        level
            .map(|level| {
                serde_json::from_value(serde_json::Value::String(level))
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))
            })
            .transpose()
    }

    pub async fn set_chat_notification_level(
        &self,
        conversation: &str,
        level: ChatNotificationLevel,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE project_conversations SET notification_level=? WHERE conversation_id=?",
        )
        .bind(level.as_str())
        .bind(conversation)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    pub async fn muted_notification_conversations(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT conversation_id FROM project_conversations WHERE notification_level='mute'",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("PRAGMA busy_timeout = 5000")
                        .execute(&mut *connection)
                        .await?;
                    // A secret answer is cleared once its request resolves;
                    // zero the space it leaves in the page as well.
                    sqlx::query("PRAGMA secure_delete = FAST")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .acquire_timeout(Duration::from_secs(5))
            .connect(database_url)
            .await?;
        // A new database starts with incremental vacuuming, set before WAL
        // mode writes its first page; older ones are converted below. Only
        // then: on an existing file the pragma waits for other writers.
        let mut connection = pool.acquire().await?;
        // Creating the file and switching it to WAL return SQLITE_BUSY at
        // once, without consulting busy_timeout, when another opener is doing
        // the same, or holds the writer for its schema upgrade. Wait as long
        // as `migrate` does for that upgrade.
        let deadline = tokio::time::Instant::now() + MIGRATION_WAIT;
        loop {
            let attempt: Result<(), sqlx::Error> = async {
                let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
                    .fetch_one(&mut *connection)
                    .await?;
                if pages == 0 {
                    sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
                        .execute(&mut *connection)
                        .await?;
                }
                sqlx::query("PRAGMA journal_mode = WAL")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            }
            .await;
            match attempt {
                Err(sqlx::Error::Database(error))
                    if error.code().as_deref().is_some_and(|code| {
                        code == "5" || code == "6" || code == "261" || code == "517"
                    }) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => break other?,
            }
        }
        drop(connection);
        let store = Self { pool };
        store.migrate().await?;
        // A failed rebuild leaves the database as it was.
        let _ = store.convert_to_incremental_vacuum().await;
        store.prune_replay().await?;
        store.forget_resolved_answers().await?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        store.run_periodic_cleanup(now_ms).await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), sqlx::Error> {
        // SQLx's SQLite migration lock is a no-op. Reserve the writer before
        // it reads applied versions, so concurrent openers cannot both decide
        // to apply the same migration. Its per-migration transactions become
        // savepoints; cancellation/failure rolls back the entire upgrade.
        let mut connection = self.pool.acquire().await?;
        // Another opener's upgrade can legitimately take longer than the
        // ordinary five-second writer wait, especially on a loaded host.
        sqlx::query("PRAGMA busy_timeout = 120000")
            .execute(&mut *connection)
            .await?;
        let upgraded = async {
            let mut migration =
                sqlx::Connection::begin_with(&mut *connection, "BEGIN IMMEDIATE").await?;
            sqlx::migrate!()
                .run_direct(None, &mut *migration, false)
                .await?;
            migration.commit().await
        }
        .await;
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&mut *connection)
            .await?;
        upgraded
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn upsert_owner_device(
        &self,
        id: &str,
        label: &str,
        public_key_jwk: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        self.upsert_owner_device_with_expiration(id, label, public_key_jwk, None, now)
            .await
    }

    pub async fn upsert_owner_device_with_expiration(
        &self,
        id: &str,
        label: &str,
        public_key_jwk: &str,
        session_expires_at_ms: Option<u64>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO devices (id, label, role, public_key_jwk, created_at, last_seen_at, session_expires_at_ms) VALUES (?, ?, 'owner', ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET label = excluded.label, public_key_jwk = excluded.public_key_jwk, last_seen_at = excluded.last_seen_at, session_expires_at_ms = excluded.session_expires_at_ms")
            .bind(id)
            .bind(label)
            .bind(public_key_jwk)
            .bind(now)
            .bind(now)
            .bind(session_expires_at_ms.map(|value| value as i64))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Only the daemon's authenticated local route may create this attribution.
    /// It has no pairing key and is excluded from restored remote identities.
    pub async fn ensure_local_desktop(&self, now: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO devices (id, label, role, public_key_jwk, created_at, is_local) VALUES ('wonder-desktop', 'This Mac', 'owner', '{}', ?, 1) ON CONFLICT(id) DO NOTHING")
            .bind(now).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn list_owner_devices(&self) -> Result<Vec<StoredDevice>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, label, public_key_jwk, created_at, last_seen_at, revoked_at, session_expires_at_ms, model FROM devices WHERE role = 'owner' AND is_local = 0 AND forgotten = 0 ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(StoredDevice {
                    created_at: row.try_get("created_at")?,
                    id: row.try_get("id")?,
                    label: row.try_get("label")?,
                    public_key_jwk: row.try_get("public_key_jwk")?,
                    last_seen_at: row.try_get("last_seen_at")?,
                    revoked_at: row.try_get("revoked_at")?,
                    session_expires_at_ms: row
                        .try_get::<Option<i64>, _>("session_expires_at_ms")?
                        .map(|value| stored_expiration(value, "session_expires_at_ms"))
                        .transpose()?,
                    model: row.try_get("model")?,
                })
            })
            .collect()
    }

    pub async fn set_owner_device_model(
        &self,
        device_id: &str,
        model: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE devices SET model = ? WHERE id = ? AND role = 'owner'")
            .bind(model)
            .bind(device_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Automation messages are still owner-visible messages, so associate
    /// them with the most recently active authorized owner device. The
    /// device id is required by the messages foreign key; automations do not
    /// create a second device identity of their own.
    pub async fn automation_message_device_id(&self) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT id FROM devices WHERE role = 'owner' AND revoked_at IS NULL ORDER BY last_seen_at DESC, created_at ASC, id ASC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn touch_owner_device(
        &self,
        device_id: &str,
        last_seen_at: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE devices SET last_seen_at = ? WHERE id = ? AND role = 'owner' AND revoked_at IS NULL",
        )
        .bind(last_seen_at)
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn rename_owner_device(
        &self,
        device_id: &str,
        label: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE devices SET label = ? WHERE id = ? AND role = 'owner' AND revoked_at IS NULL",
        )
        .bind(label)
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Pairing a device whose key the host already knows replaces that
    /// device: every other row holding the key is disconnected and forgotten
    /// (its sessions end), so the owner sees one row per phone. Returns the
    /// replaced ids; the daemon ends their live authority.
    pub async fn replace_owner_devices_with_key(
        &self,
        keep_id: &str,
        public_key_jwk: &str,
        revoked_at: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let replaced = sqlx::query_scalar::<_, String>("SELECT id FROM devices WHERE role = 'owner' AND is_local = 0 AND forgotten = 0 AND id != ? AND public_key_jwk = ?")
            .bind(keep_id)
            .bind(public_key_jwk)
            .fetch_all(&mut *transaction)
            .await?;
        for id in &replaced {
            sqlx::query("UPDATE devices SET revoked_at = COALESCE(revoked_at, ?), forgotten = 1, label = 'Removed device', public_key_jwk = '{}', last_seen_at = NULL, session_expires_at_ms = NULL WHERE id = ?")
                .bind(revoked_at)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("DELETE FROM sessions WHERE device_id = ?")
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(replaced)
    }

    pub async fn forget_revoked_device(&self, device_id: &str) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query("UPDATE devices SET forgotten = 1, label = 'Removed device', public_key_jwk = '{}', last_seen_at = NULL, session_expires_at_ms = NULL WHERE id = ? AND is_local = 0 AND revoked_at IS NOT NULL AND forgotten = 0")
            .bind(device_id).execute(&mut *transaction).await?;
        if result.rows_affected() == 1 {
            sqlx::query("DELETE FROM sessions WHERE device_id = ?")
                .bind(device_id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn revoke_owner_device(
        &self,
        device_id: &str,
        revoked_at: &str,
    ) -> Result<bool, sqlx::Error> {
        // The daemon changes in-memory authority only after this succeeds.
        // A partial commit would make a retry return false while old sessions
        // could still be authorized by that running daemon.
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query("UPDATE devices SET revoked_at = ? WHERE id = ? AND role = 'owner' AND revoked_at IS NULL")
            .bind(revoked_at)
            .bind(device_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE device_id = ?")
            .bind(device_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Permanently remove user-owned Wonder data while preserving one active
    /// owner device. This is intentionally an application-level transaction so
    /// reset callers cannot accidentally leave orphaned conversation or project
    /// records behind by deleting rows in an unsafe order.
    pub async fn reset_workspace(
        &self,
        keep_device_id: &str,
        revoked_at: &str,
    ) -> Result<WorkspaceResetResult, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let keep_exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM devices WHERE id = ? AND role = 'owner' AND revoked_at IS NULL",
        )
        .bind(keep_device_id)
        .fetch_one(&mut *transaction)
        .await?
            == 1;
        if !keep_exists {
            return Err(sqlx::Error::RowNotFound);
        }

        let revoked_device_ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM devices WHERE role = 'owner' AND revoked_at IS NULL AND id != ? ORDER BY id ASC",
        )
        .bind(keep_device_id)
        .fetch_all(&mut *transaction)
        .await?;

        sqlx::query("DELETE FROM sessions WHERE device_id != ?")
            .bind(keep_device_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("UPDATE devices SET revoked_at = ? WHERE role = 'owner' AND revoked_at IS NULL AND id != ?")
            .bind(revoked_at)
            .bind(keep_device_id)
            .execute(&mut *transaction)
            .await?;

        // Delete children before their referenced rows. Some legacy tables do
        // not declare cascading foreign keys, so being explicit keeps this
        // safe across every supported migration state.
        for statement in [
            // Outside-agent access must end atomically with the workspace reset,
            // including pending consent and refresh credentials.
            "DELETE FROM mcp_tokens",
            "DELETE FROM mcp_grants",
            "DELETE FROM mcp_authorization_codes",
            "DELETE FROM mcp_pairing_codes",
            "DELETE FROM mcp_clients",
            "DELETE FROM push_routes",
            "DELETE FROM push_outbox",
            "DELETE FROM push_intents",
            "DELETE FROM message_attachments",
            "DELETE FROM automation_runs",
            "DELETE FROM assistant_message_deltas",
            "DELETE FROM assistant_messages",
            "DELETE FROM conversation_files",
            "DELETE FROM pending_app_server_notifications",
            "DELETE FROM notification_inbox",
            "DELETE FROM app_server_notification_receipts",
            "DELETE FROM approvals",
            "DELETE FROM terminal_approval_turns",
            "DELETE FROM subagent_ownership",
            "DELETE FROM project_conversations",
            "DELETE FROM deleted_project_sessions",
            "DELETE FROM deleted_project_chats",
            "DELETE FROM project_provider_refs",
            "DELETE FROM project_roots",
            "DELETE FROM project_worktrees",
            "DELETE FROM projects",
            "DELETE FROM conversations",
            "DELETE FROM messages",
            "DELETE FROM automations",
            "DELETE FROM search_documents",
            "DELETE FROM search_document_owners",
            "DELETE FROM search_stale_conversations",
            "DELETE FROM action_nonces",
            "DELETE FROM events",
            "DELETE FROM sync_journal",
            "DELETE FROM history_entries",
            "DELETE FROM history_hydration",
        ] {
            // The statements are fixed literals; assert that audit for SQLx's
            // dynamic-query safety check without changing reset behavior.
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&mut *transaction)
                .await?;
        }

        sqlx::query("INSERT INTO sync_journal(host_epoch, occurred_at, resource, change_kind) SELECT host_epoch, ?, 'workspace', 'reset' FROM sync_state WHERE singleton = 1")
            .bind(revoked_at).execute(&mut *transaction).await?;

        transaction.commit().await?;
        Ok(WorkspaceResetResult { revoked_device_ids })
    }

    pub async fn insert_session(
        &self,
        token_hash: &str,
        device_id: &str,
        csrf_hash: &str,
        expires_at_ms: u64,
        created_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO sessions (token_hash, device_id, csrf_hash, expires_at_ms, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(token_hash)
            .bind(device_id)
            .bind(csrf_hash)
            .bind(expires_at_ms as i64)
            .bind(created_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn claim_action_nonce(
        &self,
        nonce: &str,
        used_at: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO action_nonces (nonce, used_at) VALUES (?, ?) ON CONFLICT(nonce) DO NOTHING",
        )
        .bind(nonce)
        .bind(used_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Persist the runtime's verified child/thread binding. The parent and
    /// thread fields are immutable once exposed so a reconnect cannot retarget
    /// an existing child conversation.
    #[allow(clippy::too_many_arguments)]
    pub async fn register_subagent_ownership(
        &self,
        conversation_id: &str,
        parent_conversation_id: &str,
        thread_id: &str,
        parent_thread_id: &str,
        agent_nickname: Option<&str>,
        agent_role: Option<&str>,
        agent_path: Option<&str>,
        source_json: &str,
        runtime_id: Option<&str>,
        can_accept_direct_input: Option<bool>,
        status: &str,
        is_archived: Option<bool>,
        verified_at: &str,
    ) -> Result<StoredSubagentOwnership, sqlx::Error> {
        if conversation_id.is_empty()
            || parent_conversation_id.is_empty()
            || thread_id.is_empty()
            || parent_thread_id.is_empty()
            || conversation_id == parent_conversation_id
            || thread_id == parent_thread_id
        {
            return Err(sqlx::Error::Protocol(
                "invalid subagent ownership identity".into(),
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let parent: Option<String> =
            sqlx::query_scalar("SELECT codex_thread_id FROM conversations WHERE id=?")
                .bind(parent_conversation_id)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        if parent.as_deref() != Some(parent_thread_id) {
            return Err(sqlx::Error::Protocol(
                "subagent parent thread does not match ownership".into(),
            ));
        }
        let root: Option<String> = sqlx::query_scalar(questions::VERIFIED_SUBAGENT_ROOT_SQL)
            .bind(parent_conversation_id)
            .fetch_optional(&mut *tx)
            .await?;
        let project: Option<String> = sqlx::query_scalar(
            "SELECT project_id FROM project_conversations WHERE conversation_id=?",
        )
        .bind(&root)
        .fetch_optional(&mut *tx)
        .await?;
        if project.is_none() {
            return Err(sqlx::Error::RowNotFound);
        }
        let bound: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_bindings pb JOIN runtime_bindings rb ON rb.conversation_id=? JOIN conversations rc ON rc.id=rb.conversation_id AND rc.codex_thread_id=rb.runtime_thread_id JOIN project_conversations pc ON pc.conversation_id=rb.conversation_id AND pc.agent_family=rb.agent_family AND pc.provider_store=rb.provider_store WHERE pb.conversation_id=? AND pb.runtime_thread_id=? AND pb.agent_family=rb.agent_family AND pb.provider_store=rb.provider_store)")
            .bind(&root).bind(parent_conversation_id).bind(parent_thread_id).fetch_one(&mut *tx).await?;
        if !bound {
            return Err(sqlx::Error::Protocol(
                "The parent Project runtime binding is unavailable".into(),
            ));
        }

        let existing = sqlx::query(
            "SELECT conversation_id, parent_conversation_id, thread_id, parent_thread_id, agent_nickname, agent_role, agent_path, source_json, runtime_id, can_accept_direct_input, status, is_archived FROM subagent_ownership WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let current = stored_subagent_ownership(&row);
            if current.parent_conversation_id != parent_conversation_id
                || current.thread_id != thread_id
                || current.parent_thread_id != parent_thread_id
            {
                return Err(sqlx::Error::Protocol(
                    "subagent ownership is immutable".into(),
                ));
            }
            sqlx::query(
                "UPDATE subagent_ownership SET agent_nickname = COALESCE(?, agent_nickname), agent_role = COALESCE(?, agent_role), agent_path = COALESCE(?, agent_path), source_json = ?, runtime_id = COALESCE(?, runtime_id), can_accept_direct_input = ?, status = ?, is_archived = COALESCE(?, is_archived), verified_at = ? WHERE conversation_id = ?",
            )
            .bind(agent_nickname)
            .bind(agent_role)
            .bind(agent_path)
            .bind(source_json)
            .bind(runtime_id)
            .bind(can_accept_direct_input.map(i64::from))
            .bind(status)
            .bind(is_archived.map(i64::from))
            .bind(verified_at)
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        } else {
            if sqlx::query("SELECT 1 FROM conversations WHERE id = ?")
                .bind(conversation_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_some()
            {
                return Err(sqlx::Error::Protocol(
                    "subagent conversation identity is already in use".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO conversations (id, codex_thread_id, session_id, created_at) VALUES (?, ?, NULL, ?)",
            )
            .bind(conversation_id)
            .bind(thread_id)
            .bind(verified_at)
            .execute(&mut *tx)
            .await?;
            let inherited = sqlx::query("INSERT INTO runtime_bindings(conversation_id,agent_family,runtime_thread_id,updated_at,execution_scope,provider_store) SELECT ?,agent_family,?,?,execution_scope,provider_store FROM runtime_bindings WHERE conversation_id=? AND runtime_thread_id=?")
                .bind(conversation_id).bind(thread_id).bind(verified_at).bind(parent_conversation_id).bind(parent_thread_id)
                .execute(&mut *tx).await?;
            if inherited.rows_affected() != 1 {
                return Err(sqlx::Error::Protocol(
                    "The parent runtime binding is unavailable".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO subagent_ownership (conversation_id, parent_conversation_id, thread_id, parent_thread_id, agent_nickname, agent_role, agent_path, source_json, runtime_id, can_accept_direct_input, status, is_archived, verified_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(conversation_id)
            .bind(parent_conversation_id)
            .bind(thread_id)
            .bind(parent_thread_id)
            .bind(agent_nickname)
            .bind(agent_role)
            .bind(agent_path)
            .bind(source_json)
            .bind(runtime_id)
            .bind(can_accept_direct_input.map(i64::from))
            .bind(status)
            .bind(i64::from(is_archived.unwrap_or(false)))
            .bind(verified_at)
            .execute(&mut *tx)
            .await?;
        }
        let result = sqlx::query(
            "SELECT conversation_id, parent_conversation_id, thread_id, parent_thread_id, agent_nickname, agent_role, agent_path, source_json, runtime_id, can_accept_direct_input, status, is_archived FROM subagent_ownership WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| stored_subagent_ownership(&row))
        .ok_or(sqlx::Error::RowNotFound)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn subagent_ownership_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<StoredSubagentOwnership>, sqlx::Error> {
        sqlx::query(
            "SELECT conversation_id, parent_conversation_id, thread_id, parent_thread_id, agent_nickname, agent_role, agent_path, source_json, runtime_id, can_accept_direct_input, status, is_archived FROM subagent_ownership WHERE thread_id = ?",
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| stored_subagent_ownership(&row)))
    }

    pub async fn subagent_ownership_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<StoredSubagentOwnership>, sqlx::Error> {
        sqlx::query(
            "SELECT conversation_id, parent_conversation_id, thread_id, parent_thread_id, agent_nickname, agent_role, agent_path, source_json, runtime_id, can_accept_direct_input, status, is_archived FROM subagent_ownership WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| stored_subagent_ownership(&row)))
    }

    pub async fn list_conversation_files(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<StoredConversationFile>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, conversation_id, kind, name, mime_type, byte_size, sha256, relative_path, state, additions, deletions, source_id, created_at, updated_at FROM conversation_files WHERE conversation_id = ? ORDER BY updated_at DESC")
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(stored_conversation_file).collect())
    }

    pub async fn conversation_files_by_ids(
        &self,
        conversation_id: &str,
        file_ids: &[String],
    ) -> Result<Vec<StoredConversationFile>, sqlx::Error> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }
        if file_ids.len() > 32 {
            return Err(sqlx::Error::Protocol("too_many_file_ids".into()));
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, conversation_id, kind, name, mime_type, byte_size, sha256, relative_path, state, additions, deletions, source_id, created_at, updated_at FROM conversation_files WHERE conversation_id = ",
        );
        query.push_bind(conversation_id);
        query.push(" AND id IN (");
        let mut separated = query.separated(", ");
        for file_id in file_ids {
            separated.push_bind(file_id);
        }
        separated.push_unseparated(") ORDER BY updated_at DESC LIMIT 32");
        let rows = query.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(stored_conversation_file).collect())
    }

    /// A Project's Outputs index: the recorded files of its threads, pinned
    /// first, then newest. Reads records only; availability on disk is the
    /// caller's to check.
    pub async fn project_outputs(
        &self,
        project_id: &str,
        filter: &ProjectOutputQuery<'_>,
    ) -> Result<Vec<StoredProjectOutput>, sqlx::Error> {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT f.id, f.conversation_id, f.kind, f.name, f.mime_type, f.byte_size, f.sha256, f.relative_path, f.state, f.additions, f.deletions, f.source_id, f.created_at, f.updated_at,
                c.title AS conversation_title, r.id AS replaces_file_id, (p.file_id IS NOT NULL) AS pinned
             FROM conversation_files f
             JOIN project_conversations c ON c.conversation_id = f.conversation_id
             LEFT JOIN conversation_file_pins p ON p.file_id = f.id
             LEFT JOIN conversation_files r ON r.id = f.replaces_file_id AND r.conversation_id = f.conversation_id
             WHERE f.kind IN ('attachment', 'artifact') AND c.project_id = ",
        );
        query.push_bind(project_id);
        // Owner uploads have no source; transcript pictures the owner sent
        // are `user-media`. Every other source is the agent's.
        match filter.origin {
            ProjectOutputOrigin::Agent => {
                query.push(" AND f.source_id IS NOT NULL AND f.source_id NOT LIKE 'user-media:%'");
            }
            ProjectOutputOrigin::Owner => {
                query.push(" AND (f.source_id IS NULL OR f.source_id LIKE 'user-media:%')");
            }
            ProjectOutputOrigin::Any => {}
        }
        // Pages: HTML and wonder-ui visuals (thread_tools::visual).
        const PAGES: &str =
            "lower(coalesce(f.mime_type, '')) IN ('text/html', 'application/vnd.wonder.ui+json')";
        const IMAGES: &str = "lower(coalesce(f.mime_type, '')) LIKE 'image/%'";
        match filter.file_type {
            ProjectOutputType::All => {}
            ProjectOutputType::Pages => {
                query.push(format!(" AND {PAGES}"));
            }
            ProjectOutputType::Images => {
                query.push(format!(" AND {IMAGES}"));
            }
            ProjectOutputType::Documents => {
                query.push(format!(" AND NOT {PAGES} AND NOT {IMAGES}"));
            }
        }
        if let Some(name) = filter.name.filter(|name| !name.is_empty()) {
            let escaped = name
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            query.push(" AND f.name LIKE ");
            query.push_bind(format!("%{escaped}%"));
            query.push(" ESCAPE '\\'");
        }
        if let Some(after) = &filter.after {
            query.push(" AND ((p.file_id IS NOT NULL), f.updated_at, f.id) < (");
            query.push_bind(i64::from(after.pinned));
            query.push(", ");
            query.push_bind(&after.updated_at);
            query.push(", ");
            query.push_bind(&after.id);
            query.push(")");
        }
        query.push(" ORDER BY pinned DESC, f.updated_at DESC, f.id DESC LIMIT ");
        query.push_bind(filter.limit.clamp(1, 200));
        let rows = query.build().fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|row| StoredProjectOutput {
                file: stored_conversation_file(row),
                conversation_title: row.get("conversation_title"),
                replaces_file_id: row.get("replaces_file_id"),
                pinned: row.get::<i64, _>("pinned") != 0,
            })
            .collect())
    }

    /// Pins or unpins one of a Project's outputs. False when the file is not
    /// in that Project's index.
    pub async fn set_project_output_pinned(
        &self,
        project_id: &str,
        file_id: &str,
        pinned: bool,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let listed: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversation_files f JOIN project_conversations c ON c.conversation_id = f.conversation_id WHERE f.id = ? AND c.project_id = ? AND f.kind IN ('attachment', 'artifact'))",
        )
        .bind(file_id)
        .bind(project_id)
        .fetch_one(&mut *transaction)
        .await?;
        if listed == 0 {
            return Ok(false);
        }
        if pinned {
            sqlx::query("INSERT INTO conversation_file_pins (file_id, pinned_at) VALUES (?, ?) ON CONFLICT(file_id) DO NOTHING")
                .bind(file_id)
                .bind(now)
                .execute(&mut *transaction)
                .await?;
        } else {
            sqlx::query("DELETE FROM conversation_file_pins WHERE file_id = ?")
                .bind(file_id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(true)
    }

    /// Records that `file_id` updates the earlier `replaces` in the same
    /// conversation. False when either is not that conversation's file.
    pub async fn set_conversation_file_replaces(
        &self,
        conversation_id: &str,
        file_id: &str,
        replaces: &str,
    ) -> Result<bool, sqlx::Error> {
        if file_id == replaces {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE conversation_files SET replaces_file_id = ? WHERE id = ? AND conversation_id = ? AND EXISTS(SELECT 1 FROM conversation_files WHERE id = ? AND conversation_id = ?)",
        )
        .bind(replaces)
        .bind(file_id)
        .bind(conversation_id)
        .bind(replaces)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_conversation_file(
        &self,
        id: &str,
        conversation_id: &str,
        kind: &str,
        name: &str,
        mime_type: Option<&str>,
        byte_size: Option<i64>,
        sha256: Option<&str>,
        relative_path: Option<&str>,
        state: &str,
        additions: Option<i64>,
        deletions: Option<i64>,
        source_id: Option<&str>,
        now: &str,
    ) -> Result<String, sqlx::Error> {
        // Replaying unchanged provider media must not make it look newly edited.
        // Return the persisted clock so event payloads share this same decision.
        sqlx::query_scalar("INSERT INTO conversation_files (id, conversation_id, kind, name, mime_type, byte_size, sha256, relative_path, state, additions, deletions, source_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET name = excluded.name, mime_type = excluded.mime_type, byte_size = excluded.byte_size, sha256 = excluded.sha256, relative_path = excluded.relative_path, state = excluded.state, additions = excluded.additions, deletions = excluded.deletions,
            updated_at = CASE WHEN
                (conversation_files.name, conversation_files.mime_type, conversation_files.byte_size, conversation_files.sha256, conversation_files.relative_path, conversation_files.state, conversation_files.additions, conversation_files.deletions)
                IS (excluded.name, excluded.mime_type, excluded.byte_size, excluded.sha256, excluded.relative_path, excluded.state, excluded.additions, excluded.deletions)
                THEN conversation_files.updated_at ELSE excluded.updated_at END
            RETURNING updated_at")
            .bind(id).bind(conversation_id).bind(kind).bind(name).bind(mime_type).bind(byte_size).bind(sha256).bind(relative_path)
            .bind(state).bind(additions).bind(deletions).bind(source_id).bind(now).bind(now)
            .fetch_one(&self.pool)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_scoped_automation(
        &self,
        id: &str,
        name: &str,
        kind: &str,
        scope_type: &str,
        scope_id: &str,
        conversation_id: Option<&str>,
        prompt: &str,
        rrule: &str,
        timezone: &str,
        status: &str,
        notification_policy: &str,
        next_run_at: Option<&str>,
        now: &str,
    ) -> Result<StoredAutomation, sqlx::Error> {
        // Reserve the writer before reading the target, so concurrent creates can wait.
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        match scope_type {
            "project_thread" if kind == "continuation" && conversation_id == Some(scope_id) => {
                let bound: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_conversations c JOIN projects p ON p.id=c.project_id JOIN runtime_bindings r ON r.conversation_id=c.conversation_id AND r.execution_scope='projects' AND r.agent_family=c.agent_family AND r.provider_store=c.provider_store AND r.runtime_thread_id=(SELECT codex_thread_id FROM conversations WHERE id=c.conversation_id) WHERE c.conversation_id=? AND c.native_session_id IS NOT NULL AND p.is_included=1)")
                    .bind(scope_id).fetch_one(&mut *transaction).await?;
                if bound == 0 {
                    return Err(sqlx::Error::Protocol(
                        "Project automation target is not a bound, included thread".into(),
                    ));
                }
            }
            _ => return Err(sqlx::Error::Protocol("Invalid automation target".into())),
        }
        sqlx::query("INSERT INTO automations (id, name, kind, conversation_id, prompt, rrule, timezone, status, notification_policy, scope_type, scope_id, next_run_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO NOTHING")
            .bind(id).bind(name).bind(kind).bind(conversation_id).bind(prompt)
            .bind(rrule).bind(timezone).bind(status).bind(notification_policy)
            .bind(scope_type).bind(scope_id).bind(next_run_at).bind(now).bind(now)
            .execute(&mut *transaction).await?;
        transaction.commit().await?;
        self.automation_by_id(id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn list_automations(&self) -> Result<Vec<StoredAutomation>, sqlx::Error> {
        let rows = sqlx::query("SELECT automations.*, (SELECT MAX(started_at) FROM automation_runs WHERE automation_id = automations.id) AS last_attempt_at, (SELECT MAX(finished_at) FROM automation_runs WHERE automation_id = automations.id AND status = 'completed') AS last_success_at FROM automations ORDER BY created_at ASC")
            .fetch_all(&self.pool).await?;
        rows.iter().map(stored_automation).collect()
    }

    pub async fn automation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<StoredAutomation>, sqlx::Error> {
        sqlx::query("SELECT automations.*, (SELECT MAX(started_at) FROM automation_runs WHERE automation_id = automations.id) AS last_attempt_at, (SELECT MAX(finished_at) FROM automation_runs WHERE automation_id = automations.id AND status = 'completed') AS last_success_at FROM automations WHERE id = ?")
            .bind(id).fetch_optional(&self.pool).await?.map(|row| stored_automation(&row)).transpose()
    }

    pub async fn delete_automation(&self, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM automations WHERE id = ? AND NOT EXISTS (SELECT 1 FROM automation_runs WHERE automation_id=automations.id AND status='running')")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn list_due_automations(
        &self,
        now: &str,
    ) -> Result<Vec<StoredAutomation>, sqlx::Error> {
        let rows = sqlx::query("SELECT automations.*, (SELECT MAX(started_at) FROM automation_runs WHERE automation_id = automations.id) AS last_attempt_at, (SELECT MAX(finished_at) FROM automation_runs WHERE automation_id = automations.id AND status = 'completed') AS last_success_at FROM automations WHERE status = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ? ORDER BY next_run_at ASC")
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(stored_automation).collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn claim_automation_run(
        &self,
        id: &str,
        automation_id: &str,
        scheduled_for: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("INSERT OR IGNORE INTO automation_runs (id, automation_id, scheduled_for, status, started_at) SELECT ?, ?, ?, 'running', ? WHERE NOT EXISTS (SELECT 1 FROM automation_runs WHERE automation_id = ? AND status = 'running')")
            .bind(id)
            .bind(automation_id)
            .bind(scheduled_for)
            .bind(now)
            .bind(automation_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn finish_automation_run(
        &self,
        run_id: &str,
        status: &str,
        finished_at: &str,
        error: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE automation_runs SET status = ?, finished_at = ?, error = ?, message_id = COALESCE(?, message_id) WHERE id = ?")
            .bind(status)
            .bind(finished_at)
            .bind(error)
            .bind(message_id)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_automation_run_message_id(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE automation_runs SET message_id = ?, conversation_id = (SELECT conversation_id FROM messages WHERE id = ?) WHERE id = ?")
            .bind(message_id)
            .bind(message_id)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn automation_run_for_message(
        &self,
        message_id: &str,
    ) -> Result<Option<StoredAutomationRun>, sqlx::Error> {
        sqlx::query("SELECT id, automation_id, scheduled_for, status, started_at, finished_at, error, message_id, conversation_id FROM automation_runs WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| stored_automation_run(&row)))
    }

    pub async fn list_automation_runs(
        &self,
        automation_id: &str,
    ) -> Result<Vec<StoredAutomationRun>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, automation_id, scheduled_for, status, started_at, finished_at, error, message_id, conversation_id FROM automation_runs WHERE automation_id = ? ORDER BY started_at DESC LIMIT 50")
            .bind(automation_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(stored_automation_run).collect())
    }

    pub async fn list_active_sessions(
        &self,
        now_ms: u64,
    ) -> Result<Vec<StoredSession>, sqlx::Error> {
        let rows = sqlx::query("SELECT s.token_hash, s.device_id, s.csrf_hash, s.expires_at_ms FROM sessions s JOIN devices d ON d.id = s.device_id WHERE s.expires_at_ms > ? AND d.revoked_at IS NULL")
            .bind(now_ms as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(StoredSession {
                    token_hash: row.try_get("token_hash")?,
                    device_id: row.try_get("device_id")?,
                    csrf_hash: row.try_get("csrf_hash")?,
                    expires_at_ms: stored_expiration(
                        row.try_get("expires_at_ms")?,
                        "expires_at_ms",
                    )?,
                })
            })
            .collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        now: &str,
    ) -> Result<MessageInsert, sqlx::Error> {
        self.insert_message_with_attachments(
            device_id,
            client_message_id,
            body,
            body_sha256,
            conversation_id,
            &[],
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_message_with_attachments(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        attachment_ids: &[String],
        now: &str,
    ) -> Result<MessageInsert, sqlx::Error> {
        self.insert_dispatch_message(
            device_id,
            client_message_id,
            body,
            body_sha256,
            conversation_id,
            attachment_ids,
            now,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_dispatch_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        attachment_ids: &[String],
        now: &str,
        durable_dispatch: bool,
    ) -> Result<MessageInsert, sqlx::Error> {
        self.insert_work_message(
            device_id,
            client_message_id,
            body,
            body_sha256,
            conversation_id,
            attachment_ids,
            now,
            durable_dispatch,
            None,
            None,
            &[],
        )
        .await
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_guide_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        attachment_ids: &[String],
        now: &str,
        turn: &str,
    ) -> Result<MessageInsert, sqlx::Error> {
        self.insert_work_message(
            device_id,
            client_message_id,
            body,
            body_sha256,
            conversation_id,
            attachment_ids,
            now,
            false,
            Some(turn),
            None,
            &[],
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_work_message(
        &self,
        device_id: &str,
        client_message_id: &str,
        body: &str,
        body_sha256: &str,
        conversation_id: &str,
        attachment_ids: &[String],
        now: &str,
        durable_dispatch: bool,
        guide_turn: Option<&str>,
        source: Option<&NewMessageSource<'_>>,
        skills: &[CodexSkillSelection],
    ) -> Result<MessageInsert, sqlx::Error> {
        let skills_json =
            serde_json::to_string(skills).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = sqlx::query(
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE device_id = ? AND client_message_id = ?",
        )
        .bind(device_id)
        .bind(client_message_id)
        .fetch_optional(&mut *transaction)
        .await?
        {
            let existing = stored_message(&row);
            let saved: String = sqlx::query_scalar("SELECT original_codex_skills_json FROM messages WHERE id=?").bind(&existing.id).fetch_one(&mut *transaction).await?;
            if saved != skills_json { return Ok(MessageInsert::Conflict); }
            let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dispatch_work WHERE message_id = ?")
                .bind(&existing.id).fetch_one(&mut *transaction).await?;
            let saved_guide: Option<String> = sqlx::query_scalar("SELECT expected_turn_id FROM guide_work WHERE message_id=?").bind(&existing.id).fetch_optional(&mut *transaction).await?;
            if saved_guide.as_deref() != guide_turn { return Ok(MessageInsert::Conflict); }
            if durable_dispatch != (queued != 0) {
                return Ok(MessageInsert::Conflict);
            }

            if existing.conversation_id != conversation_id || existing.body_sha256 != body_sha256 {
                transaction.commit().await?;
                return Ok(MessageInsert::Conflict);
            }
            let existing_attachments = sqlx::query(
                "SELECT file_id FROM message_attachments WHERE message_id = ? ORDER BY file_id ASC",
            )
            .bind(&existing.id)
            .fetch_all(&mut *transaction)
            .await?
            .into_iter()
            .map(|row| row.get::<String, _>("file_id"))
            .collect::<Vec<_>>();
            let mut requested_attachments = attachment_ids.to_vec();
            requested_attachments.sort();
            if existing_attachments != requested_attachments {
                transaction.commit().await?;
                return Ok(MessageInsert::Conflict);
            }
            transaction.commit().await?;
            return Ok(MessageInsert::Existing(existing));
        }

        let mut requested_attachments = attachment_ids.to_vec();
        requested_attachments.sort();
        requested_attachments.dedup();
        if !requested_attachments.is_empty() {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "SELECT COUNT(*) AS count FROM conversation_files WHERE conversation_id = ",
            );
            query.push_bind(conversation_id);
            query.push(" AND kind = 'attachment' AND state = 'available' AND id IN (");
            let mut separated = query.separated(", ");
            for file_id in &requested_attachments {
                separated.push_bind(file_id);
            }
            separated.push_unseparated(")");
            let count = query
                .build()
                .fetch_one(&mut *transaction)
                .await?
                .get::<i64, _>("count") as usize;
            if count != requested_attachments.len() {
                transaction.commit().await?;
                return Ok(MessageInsert::Conflict);
            }
            // A preview note is a single draft action. Its immutable upload
            // can be retried with the same message identity, but cannot be
            // selected for a second message. BEGIN IMMEDIATE serializes this
            // check with the message_attachments insert below.
            let mut reused = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "SELECT COUNT(*) AS count FROM conversation_files f WHERE f.conversation_id = ",
            );
            reused.push_bind(conversation_id);
            reused.push(" AND f.mime_type = ");
            reused.push_bind(ARTIFACT_ANNOTATION_MIME);
            reused.push(" AND EXISTS (SELECT 1 FROM message_attachments ma WHERE ma.file_id = f.id) AND f.id IN (");
            let mut separated = reused.separated(", ");
            for file_id in &requested_attachments {
                separated.push_bind(file_id);
            }
            separated.push_unseparated(")");
            let reused_count = reused
                .build()
                .fetch_one(&mut *transaction)
                .await?
                .get::<i64, _>("count");
            if reused_count != 0 {
                return Err(sqlx::Error::Protocol("annotation_already_sent".into()));
            }
        }

        let id = uuid::Uuid::new_v4().to_string();
        let queue_position = if durable_dispatch {
            Some(
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(MAX(m.queue_position), -1) + 1 FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.conversation_id=? AND m.state='accepted_by_wonder'",
                )
                .bind(conversation_id)
                .fetch_one(&mut *transaction)
                .await?,
            )
        } else {
            None
        };
        if let Some(queue_position) = queue_position {
            sqlx::query(
                "INSERT INTO messages (id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, queue_position) VALUES (?, ?, ?, ?, ?, ?, 'accepted_by_wonder', ?, ?)",
            )
            .bind(&id)
            .bind(device_id)
            .bind(client_message_id)
            .bind(body)
            .bind(body_sha256)
            .bind(conversation_id)
            .bind(now)
            .bind(queue_position)
            .execute(&mut *transaction)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO messages (id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at) VALUES (?, ?, ?, ?, ?, ?, 'accepted_by_wonder', ?)",
            )
            .bind(&id)
            .bind(device_id)
            .bind(client_message_id)
            .bind(body)
            .bind(body_sha256)
            .bind(conversation_id)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "UPDATE messages SET codex_skills_json=?,original_codex_skills_json=? WHERE id=?",
        )
        .bind(&skills_json)
        .bind(&skills_json)
        .bind(&id)
        .execute(&mut *transaction)
        .await?;
        for file_id in requested_attachments {
            sqlx::query(
                "INSERT INTO message_attachments (message_id, file_id, created_at) VALUES (?, ?, ?)",
            )
            .bind(&id)
            .bind(file_id)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }
        if durable_dispatch {
            // A later settings edit cannot change a queued message's speed.
            // The model may be one the owner chose for this message alone,
            // staged just before; it can belong to the other provider.
            sqlx::query("INSERT INTO message_execution_settings(message_id,model,reasoning_effort,service_tier,working_directory,agent_family) SELECT ?,CASE WHEN t.device_id IS NULL THEN p.model ELSE t.model END,CASE WHEN t.device_id IS NULL THEN p.effort ELSE t.effort END,CASE WHEN t.device_id IS NULL THEN p.service_tier ELSE t.service_tier END,p.cwd,CASE WHEN t.device_id IS NULL THEN p.agent_family ELSE t.agent_family END FROM project_conversations p LEFT JOIN project_message_targets t ON t.device_id=? AND t.client_message_id=? WHERE p.conversation_id=?")
                .bind(&id).bind(device_id).bind(client_message_id).bind(conversation_id).execute(&mut *transaction).await?;
            sqlx::query(
                "DELETE FROM project_message_targets WHERE device_id=? AND client_message_id=?",
            )
            .bind(device_id)
            .bind(client_message_id)
            .execute(&mut *transaction)
            .await?;
        }
        if durable_dispatch {
            sqlx::query("INSERT INTO dispatch_work (message_id) VALUES (?)")
                .bind(&id)
                .execute(&mut *transaction)
                .await?;
        }
        if let Some(turn) = guide_turn {
            sqlx::query("INSERT INTO guide_work(message_id,expected_turn_id) VALUES (?,?)")
                .bind(&id)
                .bind(turn)
                .execute(&mut *transaction)
                .await?;
        }
        if let Some(source) = source {
            if !Self::insert_message_source(&mut transaction, &id, source).await? {
                // A wake-up whose children were already reported creates nothing.
                return Ok(MessageInsert::Conflict);
            }
        }
        transaction.commit().await?;
        Ok(MessageInsert::Inserted(StoredMessage {
            id,
            device_id: device_id.into(),
            client_message_id: client_message_id.into(),
            body: body.into(),
            body_sha256: body_sha256.into(),
            conversation_id: conversation_id.into(),
            state: "accepted_by_wonder".into(),
            created_at: now.into(),
            codex_thread_id: None,
            codex_turn_id: None,
        }))
    }

    pub async fn attachment_ids_for_message(
        &self,
        message_id: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT file_id FROM message_attachments WHERE message_id = ? ORDER BY file_id ASC",
        )
        .bind(message_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| row.get::<String, _>("file_id"))
            .collect())
    }

    /// Forget a file the owner uploaded but never sent. A sent message keeps
    /// its files, and Mac-side media (`source_id`) is not the phone's to remove.
    /// The check and delete share one writer lock with message insertion.
    pub async fn remove_unsent_upload(
        &self,
        conversation_id: &str,
        file_id: &str,
    ) -> Result<UploadRemoval, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT id, conversation_id, kind, name, mime_type, byte_size, sha256, relative_path, state, additions, deletions, source_id, created_at, updated_at FROM conversation_files WHERE id = ? AND conversation_id = ? AND kind = 'attachment' AND source_id IS NULL")
            .bind(file_id)
            .bind(conversation_id)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(file) = row.as_ref().map(stored_conversation_file) else {
            return Ok(UploadRemoval::Missing);
        };
        let referenced: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM message_attachments WHERE file_id = ?)",
        )
        .bind(file_id)
        .fetch_one(&mut *transaction)
        .await?;
        if referenced != 0 {
            return Ok(UploadRemoval::Referenced);
        }
        sqlx::query("DELETE FROM conversation_files WHERE id = ?")
            .bind(file_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(UploadRemoval::Removed(Box::new(file)))
    }

    pub async fn attachments_for_message(
        &self,
        message_id: &str,
    ) -> Result<Vec<StoredConversationFile>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT files.id, files.conversation_id, files.kind, files.name, files.mime_type, files.byte_size, files.sha256, files.relative_path, files.state, files.additions, files.deletions, files.source_id, files.created_at, files.updated_at
             FROM conversation_files AS files
             JOIN message_attachments ON message_attachments.file_id = files.id
             WHERE message_attachments.message_id = ?
             ORDER BY message_attachments.file_id ASC",
        )
        .bind(message_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| StoredConversationFile {
                id: row.get("id"),
                conversation_id: row.get("conversation_id"),
                kind: row.get("kind"),
                name: row.get("name"),
                mime_type: row.get("mime_type"),
                byte_size: row.get("byte_size"),
                sha256: row.get("sha256"),
                relative_path: row.get("relative_path"),
                state: row.get("state"),
                additions: row.get("additions"),
                deletions: row.get("deletions"),
                source_id: row.get("source_id"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }

    pub async fn complete_message_if_active(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(sql!(
            "UPDATE messages SET state = 'completed' WHERE id = ? AND state IN ({})",
            turn_states_sql(TurnScope::Settleable)
        ))
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn complete_messages_for_codex_turn(
        &self,
        codex_turn_id: &str,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(sql!(
            "UPDATE messages SET state = 'completed' WHERE codex_turn_id = ? AND state IN ({})",
            turn_states_sql(TurnScope::Settleable)
        ))
        .bind(codex_turn_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn interrupt_message_if_active(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(sql!(
            "UPDATE messages SET state = 'interrupted' WHERE id = ? AND state IN ({})",
            turn_states_sql(TurnScope::Running)
        ))
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn update_message_delivery(
        &self,
        message_id: &str,
        state: &str,
        codex_thread_id: Option<&str>,
        codex_turn_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        self.update_message_delivery_with_error(
            message_id,
            state,
            codex_thread_id,
            codex_turn_id,
            None,
        )
        .await
    }

    /// Commit an undelivered state and its reason together. Readers must never
    /// observe a refused dispatch before the reason explaining it is durable.
    pub async fn fail_message_delivery(
        &self,
        message_id: &str,
        state: &str,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        if !matches!(state, "safe_to_retry" | "failed" | "uncertain") {
            return Err(sqlx::Error::Protocol(
                "delivery failure requires safe_to_retry, failed or uncertain".into(),
            ));
        }
        self.update_message_delivery_with_error(message_id, state, None, None, Some(reason))
            .await
    }

    async fn update_message_delivery_with_error(
        &self,
        message_id: &str,
        state: &str,
        codex_thread_id: Option<&str>,
        codex_turn_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let next = DeliveryState::from_name(state)
            .ok_or_else(|| sqlx::Error::Protocol(format!("unknown delivery state {state}")))?;
        let reason = reason.map(|reason| reason.chars().take(1000).collect::<String>());
        // Immediate, so the check below and the write see the same row.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // The one owner of message state changes: a final message never
        // returns to work, and the guard on the UPDATE keeps a concurrent
        // writer's settle from being overwritten by a late event.
        let current: Option<String> = sqlx::query_scalar("SELECT state FROM messages WHERE id = ?")
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?;
        if !current
            .as_deref()
            .and_then(DeliveryState::from_name)
            .is_some_and(|current| current.may_become(next))
        {
            return Ok(());
        }
        if codex_turn_id.is_some() {
            // A message takes its place in history when its turn starts, not
            // when it was typed: one that waited in the queue behind a long
            // reply would otherwise sort before that reply's later output and
            // drop off the newest history page, which is where the phone sees
            // that it started.
            sqlx::query("UPDATE history_entries SET sort_ms = MAX(sort_ms, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)) WHERE source = 'message' AND source_id = ?1 AND EXISTS (SELECT 1 FROM messages WHERE id = ?1 AND codex_turn_id IS NULL)")
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
        }
        let moved = sqlx::query(sql!(
            "UPDATE messages SET state = ?, codex_thread_id = COALESCE(?, codex_thread_id), codex_turn_id = COALESCE(?, codex_turn_id), delivery_error = COALESCE(?, delivery_error) WHERE id = ? AND state IN ({})",
            states_before_sql(next)
        ))
        .bind(state)
        .bind(codex_thread_id)
        .bind(codex_turn_id)
        .bind(reason.as_deref())
        .bind(message_id)
        .execute(&mut *tx)
        .await?;
        if moved.rows_affected() == 1 {
            sqlx::query("UPDATE dispatch_attempts SET phase = ?, thread_id = COALESCE(?, thread_id), turn_id = COALESCE(?, turn_id) WHERE id = (SELECT MAX(id) FROM dispatch_attempts WHERE message_id = ?)")
                .bind(state).bind(codex_thread_id).bind(codex_turn_id).bind(message_id)
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records why a message was not delivered, in plain language. Only a
    /// message that ended undelivered keeps a reason.
    pub async fn set_message_delivery_error(
        &self,
        message_id: &str,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE messages SET delivery_error = ? WHERE id = ? AND state IN ('safe_to_retry', 'failed', 'uncertain')",
        )
        .bind(reason.chars().take(1000).collect::<String>())
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn message_delivery_error(
        &self,
        message_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT delivery_error FROM messages WHERE id = ?")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await
            .map(Option::flatten)
    }

    /// Undelivered messages' reasons in one conversation, by message ID.
    pub async fn message_delivery_errors(
        &self,
        conversation_id: &str,
    ) -> Result<HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, delivery_error FROM messages WHERE conversation_id = ? AND delivery_error IS NOT NULL AND state IN ('safe_to_retry', 'failed', 'uncertain')",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn enqueue_notification(
        &self,
        notification: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO notification_inbox (notification_json) VALUES (?)")
            .bind(notification.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn notification_backlog(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT id FROM notification_inbox LIMIT 257)")
            .fetch_one(&self.pool)
            .await
    }

    pub async fn next_notification(&self) -> Result<Option<(i64, serde_json::Value)>, sqlx::Error> {
        let row =
            sqlx::query("SELECT id, notification_json FROM notification_inbox ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        row.map(|row| {
            let value = serde_json::from_str(row.get::<&str, _>("notification_json"))
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            Ok((row.get("id"), value))
        })
        .transpose()
    }

    pub async fn notification_backlog_outside_lanes(
        &self,
        excluded: &[String],
    ) -> Result<i64, sqlx::Error> {
        let excluded =
            serde_json::to_string(excluded).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query_scalar("WITH inbox AS (SELECT id, notification_json, COALESCE(json_extract(notification_json,'$.params.threadId'),json_extract(notification_json,'$.params.item.threadId')) AS thread FROM notification_inbox), lanes AS (SELECT id, notification_json, COALESCE((SELECT id FROM conversations WHERE codex_thread_id=inbox.thread LIMIT 1),'thread:' || thread,'unthreaded') AS lane FROM inbox) SELECT COUNT(*) FROM (SELECT id FROM lanes WHERE lane NOT IN (SELECT value FROM json_each(?1)) AND COALESCE('thread:' || COALESCE(json_extract(notification_json,'$.params.threadId'),json_extract(notification_json,'$.params.item.threadId')),'unthreaded') NOT IN (SELECT value FROM json_each(?1)) LIMIT 257)")
        .bind(excluded)
        .fetch_one(&self.pool)
        .await
    }

    /// Select outside lanes already owned or waiting for retry. Resolve the
    /// conversation in SQLite so different thread bindings of one chat share FIFO.
    pub async fn next_notification_outside_lanes(
        &self,
        excluded: &[String],
    ) -> Result<Option<(i64, serde_json::Value, String)>, sqlx::Error> {
        let excluded =
            serde_json::to_string(excluded).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let row = sqlx::query("WITH inbox AS (SELECT id, notification_json, COALESCE(json_extract(notification_json,'$.params.threadId'),json_extract(notification_json,'$.params.item.threadId')) AS thread FROM notification_inbox), lanes AS (SELECT id, notification_json, COALESCE((SELECT id FROM conversations WHERE codex_thread_id=inbox.thread LIMIT 1),'thread:' || thread,'unthreaded') AS lane FROM inbox) SELECT id, notification_json, lane FROM lanes WHERE lane NOT IN (SELECT value FROM json_each(?1)) AND COALESCE('thread:' || COALESCE(json_extract(notification_json,'$.params.threadId'),json_extract(notification_json,'$.params.item.threadId')),'unthreaded') NOT IN (SELECT value FROM json_each(?1)) ORDER BY id LIMIT 1")
        .bind(excluded)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let value = serde_json::from_str(row.get::<&str, _>("notification_json"))
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            Ok((row.get("id"), value, row.get("lane")))
        })
        .transpose()
    }

    /// A bounded, contiguous prefix after the head. A large envelope is a
    /// boundary, not a reason to skip ahead to later small deltas.
    pub async fn notification_prefix_after(
        &self,
        id: i64,
        limit: i64,
        max_bytes: i64,
    ) -> Result<Vec<(i64, serde_json::Value)>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, notification_json FROM (SELECT id, notification_json, SUM(length(CAST(notification_json AS BLOB))) OVER (ORDER BY id) AS bytes FROM (SELECT id, notification_json FROM notification_inbox WHERE id > ? ORDER BY id LIMIT ?)) WHERE bytes <= ? ORDER BY id")
            .bind(id).bind(limit).bind(max_bytes).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let value = serde_json::from_str(row.get::<&str, _>("notification_json"))
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                Ok((row.get("id"), value))
            })
            .collect()
    }

    pub async fn acknowledge_notifications(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        let ids = serde_json::to_string(ids).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query("DELETE FROM notification_inbox WHERE id IN (SELECT value FROM json_each(?))")
            .bind(ids)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn notification_was_started(&self, id: i64) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT started != 0 FROM notification_inbox WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn start_notification(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE notification_inbox SET started = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn acknowledge_notification(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM notification_inbox WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark the turn's receipts uncertain and fail its active assistant items.
    /// An optional (message ID, reason) is committed with that state change.
    pub async fn mark_runtime_turn_uncertain(
        &self,
        thread_id: &str,
        turn_id: &str,
        failure: Option<(&str, &str)>,
    ) -> Result<(), sqlx::Error> {
        let failed_message = failure.map(|(message, _)| message);
        let reason = failure.map(|(_, reason)| reason.chars().take(1000).collect::<String>());
        let mut transaction = self.pool.begin().await?;
        sqlx::query(sql!("UPDATE messages SET state = 'uncertain', delivery_error = CASE WHEN id = ? THEN COALESCE(?, delivery_error) ELSE delivery_error END WHERE codex_thread_id = ? AND codex_turn_id = ? AND state IN ({})", turn_states_sql(TurnScope::Settleable)))
            .bind(failed_message).bind(reason.as_deref()).bind(thread_id).bind(turn_id).execute(&mut *transaction).await?;
        sqlx::query(sql!("UPDATE assistant_messages SET state = 'failed' WHERE codex_thread_id = ? AND codex_turn_id = ? AND state IN ({})", turn_states_sql(TurnScope::Running)))
            .bind(thread_id).bind(turn_id).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn active_runtime_messages(&self) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query(sql!("SELECT * FROM messages WHERE codex_turn_id IS NOT NULL AND state IN ({}) AND NOT EXISTS (SELECT 1 FROM update_handoffs h WHERE h.thread_id=messages.codex_thread_id AND h.stopped_turn_id=messages.codex_turn_id AND h.state IN ('paused','submitting'))", turn_states_sql(TurnScope::Settleable)))
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(stored_message).collect())
    }

    pub async fn message_for_codex_turn(
        &self,
        codex_turn_id: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        sqlx::query(
            // A steer can legitimately reuse the active Codex turn id. The
            // newest Wonder message owns subsequent turn/item events while
            // the original row retains its own lifecycle state.
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE codex_turn_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(codex_turn_id)
        .fetch_optional(&self.pool)
        .await
            .map(|row| row.map(|row| stored_message(&row)))
    }

    pub async fn message_for_codex_thread_and_turn(
        &self,
        codex_thread_id: &str,
        codex_turn_id: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        sqlx::query(
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE codex_thread_id = ? AND codex_turn_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(codex_thread_id)
        .bind(codex_turn_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| stored_message(&row)))
    }

    pub async fn complete_messages_for_codex_thread_and_turn(
        &self,
        codex_thread_id: &str,
        codex_turn_id: &str,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            sql!("UPDATE messages SET state = 'completed' WHERE codex_thread_id = ? AND codex_turn_id = ? AND state IN ({})", turn_states_sql(TurnScope::Settleable)),
        )
        .bind(codex_thread_id)
        .bind(codex_turn_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn message_by_device_and_client_message_id(
        &self,
        device_id: &str,
        client_message_id: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        sqlx::query(
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE device_id = ? AND client_message_id = ?",
        )
        .bind(device_id)
        .bind(client_message_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| stored_message(&row)))
    }

    pub async fn message_by_id(
        &self,
        message_id: &str,
    ) -> Result<Option<StoredMessage>, sqlx::Error> {
        sqlx::query(
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE id = ?",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
            .map(|row| row.map(|row| stored_message(&row)))
    }

    /// Fixtures only: put a message in any state, bypassing the transition
    /// guard of `update_message_delivery`, to stage work that is running again.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn force_message_state(
        &self,
        message_id: &str,
        state: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE messages SET state = ? WHERE id = ?")
            .bind(state)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn messages_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, device_id, client_message_id, body, body_sha256, conversation_id, state, created_at, codex_thread_id, codex_turn_id FROM messages WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        let mut messages = rows
            .into_iter()
            .map(|row| stored_message(&row))
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| {
            compare_timestamps(&left.created_at, &right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(messages)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Upsert a streamed chunk idempotently using the durable notification key.
    /// The key is per notification, not per text, so two live chunks with the
    /// same text remain distinct while a replay is ignored.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_assistant_delta(
        &self,
        conversation_id: &str,
        codex_thread_id: &str,
        codex_turn_id: &str,
        item_id: &str,
        delta: &str,
        notification_key: &str,
        now: &str,
    ) -> Result<StoredAssistantMessage, sqlx::Error> {
        self.upsert_assistant_delta_with_event(
            conversation_id,
            codex_thread_id,
            codex_turn_id,
            item_id,
            delta,
            notification_key,
            now,
            None,
        )
        .await
        .map(|(message, _)| message)
    }

    /// Appends one streamed delta and, when it changed the text, journals
    /// `event` in the same transaction. The text change and the event then
    /// sit at adjacent sequences, so a snapshot read either includes both or
    /// neither and a client may apply the event to a snapshot older than it
    /// without duplicating text. Returns whether the text changed (and the
    /// event was committed).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_assistant_delta_with_event(
        &self,
        conversation_id: &str,
        codex_thread_id: &str,
        codex_turn_id: &str,
        item_id: &str,
        delta: &str,
        notification_key: &str,
        now: &str,
        event: Option<&mut HostEventEnvelope>,
    ) -> Result<(StoredAssistantMessage, bool), sqlx::Error> {
        self.upsert_assistant_deltas_with_event(
            conversation_id,
            codex_thread_id,
            codex_turn_id,
            item_id,
            &[(delta, notification_key)],
            now,
            event,
        )
        .await
    }

    /// The caller supplies an ordered, same-item prefix of the durable inbox.
    /// Receipts, per-item keys, the single text append and its event commit
    /// together. Replays (including a partially duplicated batch) cannot emit
    /// text the client already has. Never pre-claim these delta receipts.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_assistant_deltas_with_event(
        &self,
        conversation_id: &str,
        codex_thread_id: &str,
        codex_turn_id: &str,
        item_id: &str,
        deltas: &[(&str, &str)],
        now: &str,
        event: Option<&mut HostEventEnvelope>,
    ) -> Result<(StoredAssistantMessage, bool), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let id = format!("assistant:{codex_turn_id}:{item_id}");
        sqlx::query(
            "INSERT INTO assistant_messages (id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at) VALUES (?, ?, ?, ?, ?, '', 'streaming', ?, ?) ON CONFLICT(conversation_id, codex_turn_id, item_id) DO NOTHING",
        )
        .bind(&id).bind(conversation_id).bind(codex_thread_id).bind(codex_turn_id)
        .bind(item_id).bind(now).bind(now).execute(&mut *transaction).await?;
        // Do not read the entire growing transcript before every append.
        let (id, state): (String, String) = sqlx::query_as(
            "SELECT id, state FROM assistant_messages WHERE conversation_id = ? AND codex_turn_id = ? AND item_id = ?",
        ).bind(conversation_id).bind(codex_turn_id).bind(item_id)
            .fetch_one(&mut *transaction).await?;
        let mut text = String::new();
        if state != "completed" {
            for (delta, key) in deltas {
                let receipt = sqlx::query("INSERT INTO app_server_notification_receipts (notification_key, received_at) VALUES (?, ?) ON CONFLICT DO NOTHING")
                    .bind(key).bind(now).execute(&mut *transaction).await?;
                if receipt.rows_affected() == 0 || delta.is_empty() {
                    continue;
                }
                let inserted = sqlx::query("INSERT INTO assistant_message_deltas (assistant_message_id, delta_sha256) VALUES (?, ?) ON CONFLICT DO NOTHING")
                    .bind(&id).bind(key).execute(&mut *transaction).await?;
                if inserted.rows_affected() == 1 {
                    text.push_str(delta);
                }
            }
        }
        let appended = !text.is_empty();
        if appended {
            sqlx::query(
                "UPDATE assistant_messages SET text = text || ?, updated_at = ? WHERE id = ?",
            )
            .bind(&text)
            .bind(now)
            .bind(&id)
            .execute(&mut *transaction)
            .await?;
        }
        let row = sqlx::query(
            "SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE id = ?",
        ).bind(&id).fetch_one(&mut *transaction).await?;
        let result = stored_assistant_message(&row);
        if let (true, Some(event)) = (appended, event) {
            event.event = WonderEvent::AssistantDelta { text };
            self.commit_event_in(&mut transaction, event).await?;
        }
        transaction.commit().await?;
        if appended {
            self.prune_replay_after_commit().await;
        }
        Ok((result, appended))
    }

    pub async fn claim_app_server_notification(
        &self,
        notification_key: &str,
        received_at: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO app_server_notification_receipts (notification_key, received_at) VALUES (?, ?) ON CONFLICT(notification_key) DO NOTHING",
        )
        .bind(notification_key)
        .bind(received_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_pending_app_server_notification(
        &self,
        id: &str,
        identity_key: Option<&str>,
        method: &str,
        thread_id: Option<&str>,
        turn_id: Option<&str>,
        item_id: Option<&str>,
        notification_json: &str,
        params_json: &str,
        received_at: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO pending_app_server_notifications (id, identity_key, method, thread_id, turn_id, item_id, notification_json, params_json, received_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(identity_key)
        .bind(method)
        .bind(thread_id)
        .bind(turn_id)
        .bind(item_id)
        .bind(notification_json)
        .bind(params_json)
        .bind(received_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn pending_app_server_notifications_for_turn(
        &self,
        turn_id: &str,
    ) -> Result<Vec<StoredPendingAppServerNotification>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, identity_key, method, thread_id, turn_id, item_id, notification_json, params_json, received_at FROM pending_app_server_notifications WHERE turn_id = ? ORDER BY received_at ASC, rowid ASC",
        )
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| StoredPendingAppServerNotification {
                id: row.get("id"),
                identity_key: row.get("identity_key"),
                method: row.get("method"),
                thread_id: row.get("thread_id"),
                turn_id: row.get("turn_id"),
                item_id: row.get("item_id"),
                notification_json: row.get("notification_json"),
                params_json: row.get("params_json"),
                received_at: row.get("received_at"),
            })
            .collect())
    }

    pub async fn pending_app_server_notifications_for_thread_and_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Vec<StoredPendingAppServerNotification>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, identity_key, method, thread_id, turn_id, item_id, notification_json, params_json, received_at FROM pending_app_server_notifications WHERE thread_id = ? AND turn_id = ? ORDER BY received_at ASC, rowid ASC",
        )
        .bind(thread_id)
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| StoredPendingAppServerNotification {
                id: row.get("id"),
                identity_key: row.get("identity_key"),
                method: row.get("method"),
                thread_id: row.get("thread_id"),
                turn_id: row.get("turn_id"),
                item_id: row.get("item_id"),
                notification_json: row.get("notification_json"),
                params_json: row.get("params_json"),
                received_at: row.get("received_at"),
            })
            .collect())
    }

    /// Every buffered notification for a thread, oldest first.
    pub async fn pending_app_server_notifications_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<StoredPendingAppServerNotification>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, identity_key, method, thread_id, turn_id, item_id, notification_json, params_json, received_at FROM pending_app_server_notifications WHERE thread_id = ? ORDER BY received_at ASC, rowid ASC",
        )
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| StoredPendingAppServerNotification {
                id: row.get("id"),
                identity_key: row.get("identity_key"),
                method: row.get("method"),
                thread_id: row.get("thread_id"),
                turn_id: row.get("turn_id"),
                item_id: row.get("item_id"),
                notification_json: row.get("notification_json"),
                params_json: row.get("params_json"),
                received_at: row.get("received_at"),
            })
            .collect())
    }

    pub async fn delete_pending_app_server_notification(
        &self,
        id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM pending_app_server_notifications WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn release_app_server_notification(
        &self,
        notification_key: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM app_server_notification_receipts WHERE notification_key = ?")
            .bind(notification_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn complete_assistant_message(
        &self,
        conversation_id: &str,
        codex_thread_id: &str,
        codex_turn_id: &str,
        item_id: &str,
        text: &str,
        now: &str,
    ) -> Result<Option<StoredAssistantMessage>, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let id = format!("assistant:{codex_turn_id}:{item_id}");
        let existing = sqlx::query(
            "SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE conversation_id = ? AND codex_turn_id = ? AND item_id = ?",
        )
        .bind(conversation_id)
        .bind(codex_turn_id)
        .bind(item_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(existing) = existing else {
            sqlx::query(
                "INSERT INTO assistant_messages (id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'completed', ?, ?)",
            )
            .bind(&id)
            .bind(conversation_id)
            .bind(codex_thread_id)
            .bind(codex_turn_id)
            .bind(item_id)
            .bind(text)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
            let row = sqlx::query(
                "SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE id = ?",
            )
            .bind(&id)
            .fetch_one(&mut *transaction)
            .await?;
            let completed = stored_assistant_message(&row);
            transaction.commit().await?;
            return Ok(Some(completed));
        };
        let existing = stored_assistant_message(&existing);
        if !matches!(existing.state.as_str(), "accepted_by_codex" | "streaming") {
            transaction.commit().await?;
            return Ok(None);
        }
        sqlx::query(
            sql!("UPDATE assistant_messages SET text = CASE WHEN ? != '' THEN ? ELSE text END, state = 'completed', updated_at = ? WHERE id = ? AND state IN ({})", turn_states_sql(TurnScope::Running)),
        )
        .bind(text)
        .bind(text)
        .bind(now)
        .bind(&existing.id)
        .execute(&mut *transaction)
        .await?;
        let row = sqlx::query(
            "SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE id = ?",
        )
        .bind(&existing.id)
        .fetch_one(&mut *transaction)
        .await?;
        let completed = stored_assistant_message(&row);
        transaction.commit().await?;
        Ok(Some(completed))
    }

    pub async fn complete_assistant_messages_for_codex_thread_and_turn(
        &self,
        codex_thread_id: &str,
        codex_turn_id: &str,
        now: &str,
    ) -> Result<Vec<StoredAssistantMessage>, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            sql!("SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE codex_thread_id = ? AND codex_turn_id = ? AND state IN ({}) ORDER BY created_at ASC, id ASC", turn_states_sql(TurnScope::Running)),
        )
        .bind(codex_thread_id)
        .bind(codex_turn_id)
        .fetch_all(&mut *transaction)
        .await?;
        sqlx::query(
            sql!("UPDATE assistant_messages SET state = 'completed', updated_at = ? WHERE codex_thread_id = ? AND codex_turn_id = ? AND state IN ({})", turn_states_sql(TurnScope::Running)),
        )
        .bind(now)
        .bind(codex_thread_id)
        .bind(codex_turn_id)
        .execute(&mut *transaction)
        .await?;
        let completed = rows
            .into_iter()
            .map(|row| {
                let mut message = stored_assistant_message(&row);
                message.state = "completed".into();
                message.updated_at = now.into();
                message
            })
            .collect();
        transaction.commit().await?;
        Ok(completed)
    }

    pub async fn complete_assistant_messages_for_codex_turn(
        &self,
        codex_turn_id: &str,
        now: &str,
    ) -> Result<Vec<StoredAssistantMessage>, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            sql!("SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE codex_turn_id = ? AND state IN ({}) ORDER BY created_at ASC, id ASC", turn_states_sql(TurnScope::Running)),
        )
        .bind(codex_turn_id)
        .fetch_all(&mut *transaction)
        .await?;
        sqlx::query(
            sql!("UPDATE assistant_messages SET state = 'completed', updated_at = ? WHERE codex_turn_id = ? AND state IN ({})", turn_states_sql(TurnScope::Running)),
        )
        .bind(now)
        .bind(codex_turn_id)
        .execute(&mut *transaction)
        .await?;
        let completed = rows
            .into_iter()
            .map(|row| {
                let mut message = stored_assistant_message(&row);
                message.state = "completed".into();
                message.updated_at = now.into();
                message
            })
            .collect();
        transaction.commit().await?;
        Ok(completed)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn assistant_messages_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<StoredAssistantMessage>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at FROM assistant_messages WHERE conversation_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(stored_assistant_message).collect())
    }

    pub async fn claim_message_for_dispatch(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(sql!("UPDATE messages SET state = 'dispatching_to_codex' WHERE id = ? AND state = 'accepted_by_wonder' AND NOT EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=messages.conversation_id) AND (id NOT IN (SELECT message_id FROM dispatch_work) OR (NOT EXISTS (SELECT 1 FROM messages active WHERE active.conversation_id=messages.conversation_id AND active.id<>messages.id AND active.state IN ({})) AND id=(SELECT pending.id FROM messages pending JOIN dispatch_work w ON w.message_id=pending.id WHERE pending.conversation_id=messages.conversation_id AND pending.state='accepted_by_wonder' ORDER BY pending.queue_position,pending.created_at,pending.id LIMIT 1)))", turn_states_sql(TurnScope::InFlight)))
            .bind(message_id).execute(&mut *tx).await?;
        if result.rows_affected() == 1 {
            sqlx::query("INSERT INTO dispatch_attempts (message_id, phase) VALUES (?, 'claimed')")
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Commit before any turn/start bytes can reach the runtime.
    pub async fn begin_dispatch_submission(
        &self,
        message_id: &str,
        thread_id: &str,
    ) -> Result<(), sqlx::Error> {
        self.begin_dispatch_submission_with_context(message_id, thread_id, None)
            .await
    }

    pub async fn begin_dispatch_submission_with_context(
        &self,
        message_id: &str,
        thread_id: &str,
        context_json: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("UPDATE dispatch_attempts SET phase = 'submitting', thread_id = ?, context_json = ? WHERE id = (SELECT MAX(id) FROM dispatch_attempts WHERE message_id = ?) AND phase = 'claimed'")
            .bind(thread_id).bind(context_json).bind(message_id).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        sqlx::query("UPDATE messages SET codex_thread_id = ? WHERE id = ?")
            .bind(thread_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    pub async fn is_durable_dispatch(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        let row = sqlx::query(
            "SELECT EXISTS(SELECT 1 FROM dispatch_work WHERE message_id = ?) AS present",
        )
        .bind(message_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("present") != 0)
    }

    pub async fn pending_dispatch_messages(&self) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query(sql!("SELECT m.* FROM messages m JOIN dispatch_work w ON w.message_id=m.id WHERE m.state='accepted_by_wonder' AND NOT EXISTS(SELECT 1 FROM messages active WHERE active.conversation_id=m.conversation_id AND active.id<>m.id AND active.state IN ({})) AND m.id=(SELECT pending.id FROM messages pending JOIN dispatch_work pw ON pw.message_id=pending.id WHERE pending.conversation_id=m.conversation_id AND pending.state='accepted_by_wonder' ORDER BY pending.queue_position,pending.created_at,pending.id LIMIT 1) ORDER BY m.queue_position,m.created_at,m.id LIMIT 32", turn_states_sql(TurnScope::InFlight)))
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(stored_message).collect())
    }

    pub async fn ambiguous_dispatch_messages(&self) -> Result<Vec<StoredMessage>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.* FROM messages m WHERE (m.id IN (SELECT message_id FROM dispatch_work) OR m.id IN (SELECT message_id FROM guide_work)) AND m.state = 'uncertain' AND m.codex_turn_id IS NULL ORDER BY m.created_at, m.id")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(stored_message).collect())
    }

    /// Run only at service startup, before any dispatcher is allowed to claim.
    pub async fn recover_dispatch_claims(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE messages SET state = CASE WHEN (EXISTS (SELECT 1 FROM dispatch_work WHERE message_id = messages.id)) AND (SELECT phase FROM dispatch_attempts WHERE message_id = messages.id ORDER BY id DESC LIMIT 1) = 'claimed' THEN 'accepted_by_wonder' ELSE 'uncertain' END WHERE state = 'dispatching_to_codex'")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE dispatch_attempts SET phase = CASE WHEN phase = 'claimed' THEN 'released' ELSE 'uncertain' END WHERE phase IN ('claimed', 'submitting')")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE messages SET state = 'uncertain' WHERE state = 'accepted_by_wonder' AND id NOT IN (SELECT message_id FROM dispatch_work) AND id NOT IN (SELECT message_id FROM guide_work)")
            .execute(&mut *tx).await?;
        tx.commit().await
    }

    /// Releases claims no dispatch owns any more, leaving `in_flight` (claims
    /// whose dispatch is still preparing or submitting) untouched.
    pub async fn recover_direct_dispatch_claims_except(
        &self,
        in_flight: &[String],
    ) -> Result<(), sqlx::Error> {
        let in_flight = serde_json::to_string(in_flight).unwrap_or_else(|_| "[]".into());
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE messages SET state = CASE WHEN EXISTS (SELECT 1 FROM dispatch_work WHERE message_id = messages.id) AND (SELECT phase FROM dispatch_attempts WHERE message_id = messages.id ORDER BY id DESC LIMIT 1) = 'claimed' THEN 'accepted_by_wonder' ELSE 'uncertain' END WHERE state = 'dispatching_to_codex' AND id IN (SELECT message_id FROM dispatch_work) AND id NOT IN (SELECT value FROM json_each(?))")
            .bind(&in_flight).execute(&mut *tx).await?;
        sqlx::query("UPDATE dispatch_attempts SET phase = CASE WHEN phase = 'claimed' THEN 'released' ELSE 'uncertain' END WHERE phase IN ('claimed', 'submitting') AND message_id IN (SELECT message_id FROM dispatch_work) AND message_id NOT IN (SELECT value FROM json_each(?))")
            .bind(&in_flight).execute(&mut *tx).await?;
        tx.commit().await
    }

    pub async fn requeue_message_for_retry(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE messages SET state = 'accepted_by_wonder', delivery_error = NULL WHERE id = ? AND state IN ('safe_to_retry', 'failed')",
        )
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn conversation_thread(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query("SELECT codex_thread_id FROM conversations WHERE id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| row.try_get("codex_thread_id"))
            .transpose()
    }

    pub async fn conversation_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT id FROM conversations WHERE codex_thread_id = ? LIMIT 1")
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn insert_pending_approval(
        &self,
        server_request_id: &str,
        method: &str,
        params_json: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let action_nonce = uuid::Uuid::new_v4().to_string();
        let params = serde_json::from_str::<serde_json::Value>(params_json).unwrap_or_default();
        let thread_id = params
            .get("threadId")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let turn_id = params
            .get("turnId")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let item_id = params
            .get("itemId")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        // The chat the phone lists the request under, so its journal entries
        // invalidate that chat alone. Unknown stays NULL: a host-wide change.
        let owner = match self.approval_push_conversation(thread_id, turn_id).await? {
            Some(owner) => Some(owner),
            // Matching a ChatGPT request needs its stored row, not yet written.
            None if server_request_id.starts_with("chatgpt:") => {
                self.chatgpt_thread_conversation(thread_id).await?
            }
            None => None,
        };
        sqlx::query(
            "INSERT INTO approvals (approval_id, server_request_id, method, params_json, thread_id, turn_id, item_id, action_nonce, state, created_at, conversation_id) SELECT ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ? WHERE NOT EXISTS (SELECT 1 FROM terminal_approval_turns WHERE runtime_id = ? AND thread_id = ? AND turn_id = ?) ON CONFLICT(server_request_id) DO NOTHING",
        )
        .bind(server_request_id)
        .bind(server_request_id)
        .bind(method)
        .bind(params_json)
        .bind(thread_id)
        .bind(turn_id)
        .bind(item_id)
        .bind(action_nonce)
        .bind(now)
        .bind(owner)
        .bind(params.get("_wonderRuntimeId").and_then(serde_json::Value::as_str))
        .bind(thread_id)
        .bind(turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `chatgpt_request_conversation`'s owner for a request not stored yet,
    /// resolved to the Project conversation `approval_push_conversation` names.
    async fn chatgpt_thread_conversation(
        &self,
        thread: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let owners: Vec<String> = sqlx::query_scalar("SELECT conversation_id FROM project_conversations WHERE native_session_id=? AND agent_family='codex' AND wonder_archived_at IS NULL LIMIT 2")
            .bind(thread).fetch_all(&self.pool).await?;
        let [owner] = owners.as_slice() else {
            return Ok(None);
        };
        Ok(self
            .owning_project_conversation(owner)
            .await?
            .map(|owner| owner.conversation_id))
    }

    pub async fn list_pending_approvals(&self) -> Result<Vec<StoredApproval>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT approval_id, server_request_id, method, params_json, thread_id, turn_id, item_id, action_nonce, state, decision, resolution_idempotency_key, resolution_body_sha256, resolution_json FROM approvals WHERE state IN ('pending', 'resolving') ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| stored_approval(&row)).collect())
    }

    pub async fn approval(&self, id: &str) -> Result<Option<StoredApproval>, sqlx::Error> {
        let row = sqlx::query("SELECT approval_id, server_request_id, method, params_json, thread_id, turn_id, item_id, action_nonce, state, decision, resolution_idempotency_key, resolution_body_sha256, resolution_json FROM approvals WHERE approval_id = ?")
            .bind(id).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| stored_approval(&row)))
    }

    pub async fn begin_approval_resolution(
        &self,
        approval_id: &str,
    ) -> Result<Option<StoredApproval>, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE approvals SET state = 'resolving' WHERE approval_id = ? AND state = 'pending' AND NOT EXISTS (SELECT 1 FROM terminal_approval_turns t WHERE t.runtime_id=json_extract(approvals.params_json,'$._wonderRuntimeId') AND t.thread_id=approvals.thread_id AND t.turn_id=approvals.turn_id)",
        )
        .bind(approval_id)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            transaction.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT approval_id, server_request_id, method, params_json, thread_id, turn_id, item_id, action_nonce, state, decision, resolution_idempotency_key, resolution_body_sha256, resolution_json FROM approvals WHERE approval_id = ?",
        )
        .bind(approval_id)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(Some(stored_approval(&row)))
    }

    pub async fn resume_approval_resolution(
        &self,
        approval_id: &str,
    ) -> Result<Option<StoredApproval>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT approval_id, server_request_id, method, params_json, thread_id, turn_id, item_id, action_nonce, state, decision, resolution_idempotency_key, resolution_body_sha256, resolution_json FROM approvals WHERE approval_id = ? AND state = 'resolving' AND NOT EXISTS (SELECT 1 FROM terminal_approval_turns t WHERE t.runtime_id=json_extract(approvals.params_json,'$._wonderRuntimeId') AND t.thread_id=approvals.thread_id AND t.turn_id=approvals.turn_id)",
        )
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| stored_approval(&row)))
    }

    pub async fn finish_approval_resolution(
        &self,
        approval_id: &str,
        decision: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE approvals SET state = 'resolved', decision = ?, resolved_at = ?, resolution_json = NULL WHERE approval_id = ? AND state = 'resolving'",
        )
        .bind(decision)
        .bind(now)
        .bind(approval_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn persist_approval_resolution_intent(
        &self,
        approval_id: &str,
        idempotency_key: &str,
        body_sha256: &str,
        response_json: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE approvals SET resolution_idempotency_key = ?, resolution_body_sha256 = ?, resolution_json = ? WHERE approval_id = ? AND state = 'resolving' AND (resolution_idempotency_key IS NULL OR resolution_idempotency_key = ?)",
        )
        .bind(idempotency_key)
        .bind(body_sha256)
        .bind(response_json)
        .bind(approval_id)
        .bind(idempotency_key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn reset_approval_resolution(&self, approval_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE approvals SET state = 'pending' WHERE approval_id = ? AND state = 'resolving'",
        )
        .bind(approval_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn resolve_approval_by_server_request_id(
        &self,
        server_request_id: &str,
        decision: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE approvals SET state = 'resolved', decision = ?, resolved_at = ?, resolution_json = NULL WHERE server_request_id = ? AND state IN ('pending', 'resolving')",
        )
        .bind(decision)
        .bind(now)
        .bind(server_request_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Legacy history insertion (including old fixtures). Daemon publishers use
    /// `commit_event` so the committed sync journal receives the event too.
    pub async fn append_event(&self, event: &HostEventEnvelope) -> Result<(), sqlx::Error> {
        let payload_json =
            serde_json::to_string(event).map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        sqlx::query("INSERT INTO events (event_id, host_epoch, sequence, occurred_at, payload_json) VALUES (?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING")
            .bind(&event.event_id)
            .bind(&event.host_epoch)
            .bind(event.sequence as i64)
            .bind(&event.occurred_at)
            .bind(payload_json)
            .execute(&self.pool)
            .await?;
        self.prune_replay().await?;
        Ok(())
    }

    pub async fn events_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<HostEventEnvelope>, sqlx::Error> {
        let rows = sqlx::query("SELECT payload_json FROM events WHERE json_extract(payload_json, '$.conversationId') = ? ORDER BY sequence ASC")
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await?;
        let mut events = Vec::new();
        for row in rows {
            let payload: String = row.get("payload_json");
            let event = serde_json::from_str::<HostEventEnvelope>(&payload)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            if event.conversation_id.as_deref() == Some(conversation_id) {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn stored_message(row: &sqlx::sqlite::SqliteRow) -> StoredMessage {
    StoredMessage {
        id: row.get("id"),
        device_id: row.get("device_id"),
        client_message_id: row.get("client_message_id"),
        body: row.get("body"),
        body_sha256: row.get("body_sha256"),
        conversation_id: row.get("conversation_id"),
        state: row.get("state"),
        created_at: row.get("created_at"),
        codex_thread_id: row.get("codex_thread_id"),
        codex_turn_id: row.get("codex_turn_id"),
    }
}

fn stored_assistant_message(row: &sqlx::sqlite::SqliteRow) -> StoredAssistantMessage {
    StoredAssistantMessage {
        id: row.get("id"),
        conversation_id: row.get("conversation_id"),
        codex_thread_id: row.get("codex_thread_id"),
        codex_turn_id: row.get("codex_turn_id"),
        item_id: row.get("item_id"),
        text: row.get("text"),
        state: row.get("state"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn stored_conversation_file(row: &sqlx::sqlite::SqliteRow) -> StoredConversationFile {
    StoredConversationFile {
        id: row.get("id"),
        conversation_id: row.get("conversation_id"),
        kind: row.get("kind"),
        name: row.get("name"),
        mime_type: row.get("mime_type"),
        byte_size: row.get("byte_size"),
        sha256: row.get("sha256"),
        relative_path: row.get("relative_path"),
        state: row.get("state"),
        additions: row.get("additions"),
        deletions: row.get("deletions"),
        source_id: row.get("source_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn stored_subagent_ownership(row: &sqlx::sqlite::SqliteRow) -> StoredSubagentOwnership {
    StoredSubagentOwnership {
        conversation_id: row.get("conversation_id"),
        parent_conversation_id: row.get("parent_conversation_id"),
        thread_id: row.get("thread_id"),
        parent_thread_id: row.get("parent_thread_id"),
        agent_nickname: row.get("agent_nickname"),
        agent_role: row.get("agent_role"),
        agent_path: row.get("agent_path"),
        source_json: row.get("source_json"),
        runtime_id: row.get("runtime_id"),
        can_accept_direct_input: row
            .get::<Option<i64>, _>("can_accept_direct_input")
            .map(|value| value != 0),
        status: row.get("status"),
        is_archived: row.get::<i64, _>("is_archived") != 0,
    }
}

fn stored_approval(row: &sqlx::sqlite::SqliteRow) -> StoredApproval {
    StoredApproval {
        approval_id: row.get("approval_id"),
        server_request_id: row.get("server_request_id"),
        method: row.get("method"),
        params_json: row.get("params_json"),
        action_nonce: row.get("action_nonce"),
        state: row.get("state"),
        decision: row.get("decision"),
        thread_id: row.get("thread_id"),
        turn_id: row.get("turn_id"),
        item_id: row.get("item_id"),
        resolution_idempotency_key: row.get("resolution_idempotency_key"),
        resolution_body_sha256: row.get("resolution_body_sha256"),
        resolution_json: row.get("resolution_json"),
    }
}

fn stored_automation(row: &sqlx::sqlite::SqliteRow) -> Result<StoredAutomation, sqlx::Error> {
    // Migration tests load pre-0086 rows before upgrading their database.
    let revision = match row.try_get("revision") {
        Ok(revision) => revision,
        Err(sqlx::Error::ColumnNotFound(_)) => 0,
        Err(error) => return Err(error),
    };
    let scope_type: String = row.get("scope_type");
    let scope_id: Option<String> = row.get("scope_id");
    let target = match (scope_type.as_str(), scope_id) {
        ("project_thread", Some(scope_id)) => AutomationTarget::ProjectThread { scope_id },
        _ => return Err(sqlx::Error::Protocol("Invalid automation target".into())),
    };
    Ok(StoredAutomation {
        id: row.get("id"),
        revision,
        name: row.get("name"),
        kind: row.get("kind"),
        target,
        conversation_id: row.get("conversation_id"),
        prompt: row.get("prompt"),
        rrule: row.get("rrule"),
        timezone: row.get("timezone"),
        status: row.get("status"),
        notification_policy: row.get("notification_policy"),
        next_run_at: row.get("next_run_at"),
        last_run_at: row.get("last_run_at"),
        last_attempt_at: row.get("last_attempt_at"),
        last_success_at: row.get("last_success_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn stored_automation_run(row: &sqlx::sqlite::SqliteRow) -> StoredAutomationRun {
    StoredAutomationRun {
        id: row.get("id"),
        automation_id: row.get("automation_id"),
        scheduled_for: row.get("scheduled_for"),
        status: row.get("status"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        error: row.get("error"),
        message_id: row.get("message_id"),
        conversation_id: row.get("conversation_id"),
    }
}

#[cfg(test)]
async fn seed_project_thread(
    store: &Store,
    id: &str,
    title: &str,
    cwd: &str,
    model: Option<&str>,
    effort: Option<&str>,
    now: &str,
) -> Result<StoredProjectConversation, sqlx::Error> {
    let project_id = format!("project-{id}");
    store
        .create_project(
            &project_id,
            &project_id,
            "fixture",
            title,
            &[ProjectRootInput {
                path: cwd.into(),
                canonical_path: cwd.into(),
            }],
            0,
            now,
        )
        .await?;
    let family = if model.is_some_and(|m| m.starts_with("claude:")) {
        AgentFamily::Claude
    } else {
        AgentFamily::Codex
    };
    store
        .create_project_conversation(ProjectConversationInsert {
            conversation_id: id,
            project_id: &project_id,
            family,
            provider_store: "fixture-store",
            native_session_id: None,
            cwd,
            roots_revision: 1,
            title,
            model,
            effort,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now,
        })
        .await?;
    store
        .project_conversation(id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

#[cfg(test)]
async fn bind_project_fixture(
    store: &Store,
    id: &str,
    family: AgentFamily,
    thread: &str,
    session: Option<&str>,
    now: &str,
) -> Result<(), sqlx::Error> {
    if store.project_conversation(id).await?.is_none() {
        seed_project_thread(
            store,
            id,
            "Project",
            "/tmp",
            if family == AgentFamily::Claude {
                Some("claude:haiku")
            } else {
                None
            },
            None,
            now,
        )
        .await?;
    }
    store.set_project_native_session(id, thread, now).await?;
    store
        .bind_project_runtime(id, family, "fixture-store", thread, session, now)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contract: two branches that each add a migration cannot share a version;
    // sqlx would fail every Store open with a UNIQUE constraint error.
    #[test]
    fn migration_versions_are_unique() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut seen = std::collections::HashMap::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let version = name.split('_').next().unwrap().to_owned();
            if let Some(other) = seen.insert(version, name.clone()) {
                panic!("migrations {other} and {name} share a version; renumber the newer one");
            }
        }
    }

    // Contract: an approval or automation run journals the chat that owns it,
    // including an answer recorded without its chat, so a phone refreshes that
    // chat alone. A request with no established owner stays host-wide.
    #[tokio::test]
    async fn approvals_and_automation_runs_journal_their_owning_chat() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        bind_project_fixture(&store, "parent", AgentFamily::Codex, "thread", None, "now")
            .await
            .unwrap();
        seed_project_thread(&store, "other", "Other", "/tmp", None, None, "now")
            .await
            .unwrap();
        store
            .upsert_owner_device("phone", "phone", "{}", "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message(
                "phone",
                "client",
                "hello",
                "hash",
                "parent",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("message was not inserted")
        };
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission(&message.id, "thread")
            .await
            .unwrap();
        let params = |thread: &str| format!(r#"{{"threadId":"{thread}","turnId":"turn"}}"#);
        let journal = |resource: &'static str| {
            let pool = store.pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>(
                    "SELECT conversation_id FROM sync_journal WHERE resource=? ORDER BY sequence",
                )
                .bind(resource)
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        for (request, thread) in [("owned", "thread"), ("stray", "unknown-thread")] {
            store
                .insert_pending_approval(
                    request,
                    "item/commandExecution/requestApproval",
                    &params(thread),
                    "now",
                )
                .await
                .unwrap();
        }
        assert_eq!(journal("approvals").await, [Some("parent".into()), None]);
        sqlx::query("UPDATE approvals SET state='resolved' WHERE approval_id='owned'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM approvals WHERE approval_id='owned'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            journal("approvals").await[2..],
            [Some("parent".into()), Some("parent".into())]
        );

        // A phone answer is published with no chat or turn of its own.
        store
            .insert_pending_approval(
                "answered",
                "item/commandExecution/requestApproval",
                &params("thread"),
                "now",
            )
            .await
            .unwrap();
        for (request, owner) in [("answered", Some("parent")), ("stray", None)] {
            let mut event = HostEventEnvelope {
                event_id: uuid::Uuid::new_v4().to_string(),
                host_epoch: "epoch".into(),
                sequence: 0,
                occurred_at: "now".into(),
                request_id: Some(request.into()),
                device_id: None,
                conversation_id: None,
                message_id: None,
                thread_id: None,
                turn_id: None,
                item_id: None,
                approval_id: Some(request.into()),
                event: WonderEvent::ApprovalResolved {
                    request_id: request.into(),
                    decision: "accept".into(),
                },
            };
            store.commit_event(&mut event).await.unwrap();
            assert_eq!(event.conversation_id.as_deref(), owner, "{request}");
            let journaled: Option<String> =
                sqlx::query_scalar("SELECT conversation_id FROM sync_journal WHERE sequence=?")
                    .bind(event.sequence as i64)
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(journaled.as_deref(), owner, "{request}");
        }

        // A run names its automation's chat before its message exists.
        sqlx::query("INSERT INTO automations (id, name, kind, conversation_id, prompt, rrule, timezone, status, notification_policy, scope_type, scope_id, created_at, updated_at) VALUES ('automation', 'Check in', 'continuation', 'other', 'Check in', 'FREQ=DAILY', 'UTC', 'active', 'all_runs', 'project_thread', 'other', 'now', 'now')")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .claim_automation_run("run", "automation", "now", "now")
            .await
            .unwrap());
        store
            .finish_automation_run("run", "completed", "now", None, None)
            .await
            .unwrap();
        assert_eq!(
            journal("automation_runs").await,
            [Some("other".into()), Some("other".into())]
        );

        // The run's saved chat takes precedence over both the message and
        // automation. Deletes must use the old run, including its fallback.
        sqlx::query(
            "UPDATE automation_runs SET conversation_id='other', message_id=? WHERE id='run'",
        )
        .bind(&message.id)
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            journal("automation_runs").await.last().unwrap().as_deref(),
            Some("other")
        );
        let mutations = [
            ("UPDATE automation_runs SET conversation_id='parent' WHERE id='run'", Some("parent")),
            ("UPDATE automation_runs SET conversation_id=NULL WHERE id='run'", Some("parent")),
            ("DELETE FROM automation_runs WHERE id='run'", Some("parent")),
            ("INSERT INTO automation_runs (id, automation_id, scheduled_for, status, started_at, conversation_id) VALUES ('saved-run', 'automation', 'later', 'running', 'now', 'parent')", Some("parent")),
            ("DELETE FROM automation_runs WHERE id='saved-run'", Some("parent")),
            ("INSERT INTO automation_runs (id, automation_id, scheduled_for, status, started_at) VALUES ('unowned-run', 'automation', 'last', 'running', 'now')", Some("other")),
            ("DELETE FROM automation_runs WHERE id='unowned-run'", Some("other")),
        ];
        for (statement, owner) in mutations {
            sqlx::query(statement).execute(&store.pool).await.unwrap();
            assert_eq!(
                journal("automation_runs").await.last().unwrap().as_deref(),
                owner,
                "{statement}"
            );
        }
        // A legacy run without its own chat loses the automation fallback
        // when deleting the automation cascades to its runs.
        assert!(store
            .claim_automation_run("unknown-run", "automation", "unknown", "now")
            .await
            .unwrap());
        assert_eq!(
            journal("automation_runs").await.last().unwrap().as_deref(),
            Some("other")
        );
        sqlx::query("DELETE FROM automations WHERE id='automation'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(journal("automation_runs").await.last(), Some(&None));
    }

    // Contract: upgrade keeps chat content; all policies survive closing/reopening SQLite.
    #[tokio::test]
    async fn chat_notification_migration_preserves_content_and_restart_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("state.db").display()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 145)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        let store = Store { pool };
        seed_project_thread(
            &store,
            "existing",
            "Keep this title",
            "/tmp",
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        store
            .upsert_owner_device("phone", "Phone", "{}", "now")
            .await
            .unwrap();
        store
            .insert_message(
                "phone",
                "receipt",
                "Keep this message",
                "hash",
                "existing",
                "now",
            )
            .await
            .unwrap();
        migrations.run(&store.pool).await.unwrap();
        assert_eq!(
            store.chat_notification_level("existing").await.unwrap(),
            Some(ChatNotificationLevel::Default)
        );
        assert_eq!(
            store.chat_notification_level("missing").await.unwrap(),
            None
        );
        assert!(!store
            .set_chat_notification_level("missing", ChatNotificationLevel::Follow)
            .await
            .unwrap());
        assert!(
            sqlx::query("UPDATE project_conversations SET notification_level='invalid'")
                .execute(&store.pool)
                .await
                .is_err()
        );
        store.pool.close().await;
        for level in [
            ChatNotificationLevel::Default,
            ChatNotificationLevel::Follow,
            ChatNotificationLevel::Mute,
        ] {
            let store = Store::connect(&url).await.unwrap();
            store
                .set_chat_notification_level("existing", level)
                .await
                .unwrap();
            store.pool.close().await;
            let reopened = Store::connect(&url).await.unwrap();
            assert_eq!(
                reopened.chat_notification_level("existing").await.unwrap(),
                Some(level)
            );
            assert_eq!(
                reopened
                    .project_conversation("existing")
                    .await
                    .unwrap()
                    .unwrap()
                    .title,
                "Keep this title"
            );
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT body FROM messages WHERE conversation_id='existing'"
                )
                .fetch_one(&reopened.pool)
                .await
                .unwrap(),
                "Keep this message"
            );
            reopened.pool.close().await;
        }
    }

    // Contract: an upgraded host drops the never-shipped voice transcription
    // tables, rows and all; a new host never has them.
    #[tokio::test]
    async fn upgrade_drops_the_unshipped_transcription_tables() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 123)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            "INSERT INTO transcriptions(id,state,source_device_id,duration_ms,created_at,updated_at) VALUES ('t','queued','phone',1,'then','then');
             INSERT INTO asr_cancelled_requests VALUES ('phone','request','then');",
        )
        .execute(&pool)
        .await
        .unwrap();
        migrations.run(&pool).await.unwrap();
        let tables = |pool: SqlitePool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM sqlite_master WHERE name IN ('transcriptions','asr_cancelled_requests','transcription_request_identity')",
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        assert_eq!(tables(pool).await, Vec::<String>::new());
        let fresh = Store::connect("sqlite::memory:").await.unwrap();
        assert_eq!(tables(fresh.pool.clone()).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn user_echo_repair_preserves_real_assistant_with_identical_text() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        for item in ["user-item", "assistant-item"] {
            store
                .complete_assistant_message("chat", "thread", "turn", item, "hello", "now")
                .await
                .unwrap();
        }
        let payload = serde_json::json!({"conversationId":"chat", "threadId":"thread", "turnId":"turn", "itemId":"user-item", "event":{"data":{"category":"thread_item_upsert", "detail":serde_json::json!({"item":{"type":"userMessage"}}).to_string()}}});
        sqlx::query("INSERT INTO events VALUES ('event','epoch',1,'now',?)")
            .bind(payload.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0040_user_echo_projection.sql"))
            .execute(&store.pool)
            .await
            .unwrap();
        let rows = store
            .assistant_messages_for_conversation("chat")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_id, "assistant-item");
        assert_eq!(rows[0].text, "hello");
    }

    #[tokio::test]
    async fn local_desktop_is_not_a_remote_pairing_identity() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.ensure_local_desktop("now").await.unwrap();
        store.ensure_local_desktop("later").await.unwrap();
        assert!(store.list_owner_devices().await.unwrap().is_empty());
        assert!(store
            .insert_dispatch_message(
                "wonder-desktop",
                "request",
                "hello",
                "hash",
                "chat",
                &[],
                "now",
                true
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn update_readiness_rejects_inflight_submission_and_computer_control() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert!(!store.has_unsafe_update_work("host").await.unwrap());
        store
            .upsert_owner_device("device", "Owner", "{}", "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message(
                "device",
                "client",
                "body",
                "hash",
                "project",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("message insertion");
        };
        assert!(!store.has_unsafe_update_work("host").await.unwrap());
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        assert!(store.has_unsafe_update_work("host").await.unwrap());
        store
            .update_message_delivery(&message.id, "uncertain", None, None)
            .await
            .unwrap();
        assert!(!store.has_unsafe_update_work("host").await.unwrap());
        store
            .update_message_delivery(&message.id, "completed", None, None)
            .await
            .unwrap();
        assert!(!store.has_unsafe_update_work("host").await.unwrap());
        sqlx::query("INSERT INTO computer_sessions(id,client_request_id,owner_device_id,host_installation_id,conversation_id,generation,state,geometry_revision,created_at,updated_at,last_state_at) VALUES ('session','request','device','host','project',1,'paused',0,'now','now','now')")
            .execute(&store.pool).await.unwrap();
        assert!(store.has_unsafe_update_work("host").await.unwrap());
        assert!(!store.has_unsafe_update_work("other-host").await.unwrap());
        sqlx::query("UPDATE computer_sessions SET state='ended' WHERE id='session'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO computer_control_leases(id,client_request_id,session_id,owner_device_id,host_installation_id,conversation_id,session_generation,geometry_revision,status,acquired_at,updated_at,expires_at) VALUES ('control','control-request','session','device','host','project',1,0,'active','now','now','later')")
            .execute(&store.pool).await.unwrap();
        assert!(store.has_unsafe_update_work("host").await.unwrap());
        sqlx::query("UPDATE computer_control_leases SET status='released' WHERE id='control'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(!store.has_unsafe_update_work("host").await.unwrap());
    }

    #[tokio::test]
    async fn durable_acceptance_claim_and_submission_boundaries_are_atomic() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("device", "Owner", "{}", "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message(
                "device",
                "client",
                "body",
                "hash",
                "project",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        assert_eq!(
            store.pending_dispatch_messages().await.unwrap(),
            vec![message.clone()]
        );
        assert!(matches!(
            store
                .insert_dispatch_message(
                    "device",
                    "client",
                    "body",
                    "hash",
                    "project",
                    &[],
                    "now",
                    true
                )
                .await
                .unwrap(),
            MessageInsert::Existing(_)
        ));
        assert_eq!(
            store
                .insert_message("device", "client", "body", "hash", "project", "now")
                .await
                .unwrap(),
            MessageInsert::Conflict
        );
        let (first, second) = tokio::join!(
            store.claim_message_for_dispatch(&message.id),
            store.claim_message_for_dispatch(&message.id)
        );
        assert_eq!(u8::from(first.unwrap()) + u8::from(second.unwrap()), 1);
        store.recover_dispatch_claims().await.unwrap();
        assert_eq!(store.pending_dispatch_messages().await.unwrap().len(), 1);
        assert!(store.claim_message_for_dispatch(&message.id).await.unwrap());
        store
            .begin_dispatch_submission_with_context(
                &message.id,
                "thread",
                Some(r#"{"instructions":"original"}"#),
            )
            .await
            .unwrap();
        assert!(store
            .begin_dispatch_submission_with_context(
                &message.id,
                "thread",
                Some(r#"{"instructions":"changed"}"#)
            )
            .await
            .is_err());
        let saved: String = sqlx::query_scalar(
            "SELECT context_json FROM dispatch_attempts ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(saved, r#"{"instructions":"original"}"#);
        store.recover_dispatch_claims().await.unwrap();
        assert!(store.pending_dispatch_messages().await.unwrap().is_empty());
        assert!(!store.requeue_message_for_retry(&message.id).await.unwrap());
        let phases: Vec<String> =
            sqlx::query_scalar("SELECT phase FROM dispatch_attempts ORDER BY id")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(phases, vec!["released", "uncertain"]);
    }

    #[tokio::test]
    async fn failed_queue_insert_rolls_back_message_acceptance() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("device", "Owner", "{}", "now")
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER refuse_queue BEFORE INSERT ON dispatch_work BEGIN SELECT RAISE(ABORT, 'disk fault'); END").execute(&store.pool).await.unwrap();
        assert!(store
            .insert_dispatch_message(
                "device",
                "client",
                "body",
                "hash",
                "project",
                &[],
                "now",
                true
            )
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn duplicate_message_is_idempotent_and_conflicting_body_is_rejected() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device(
                "device-1",
                "Recorded",
                "{\"kty\":\"EC\"}",
                "2026-08-31T00:00:00Z",
            )
            .await
            .expect("device");
        let first = store
            .insert_message(
                "device-1",
                "client-1",
                "message body",
                "hash-a",
                "conversation-1",
                "2026-08-31T00:00:00Z",
            )
            .await
            .expect("insert");
        let second = store
            .insert_message(
                "device-1",
                "client-1",
                "message body",
                "hash-a",
                "conversation-1",
                "2026-08-31T00:00:01Z",
            )
            .await
            .expect("duplicate");
        let conflict = store
            .insert_message(
                "device-1",
                "client-1",
                "different body",
                "hash-b",
                "conversation-1",
                "2026-08-31T00:00:02Z",
            )
            .await
            .expect("conflict");
        assert!(matches!(first, MessageInsert::Inserted(_)));
        assert!(matches!(second, MessageInsert::Existing(_)));
        assert_eq!(conflict, MessageInsert::Conflict);
    }

    #[tokio::test]
    async fn owner_devices_and_active_sessions_survive_store_reopen() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        // Reopening drops sessions already expired, so this one is still live.
        const EXPIRES_AT_MS: u64 = 4_000_000_000_000;
        let store = Store::connect(&database_url).await.expect("store");
        store
            .upsert_owner_device("device-1", "Chrome", "{\"kty\":\"EC\"}", "now")
            .await
            .expect("device");
        store
            .insert_session("token-hash", "device-1", "csrf-hash", EXPIRES_AT_MS, "now")
            .await
            .expect("session");
        drop(store);
        let reopened = Store::connect(&database_url).await.expect("reopen");
        let devices = reopened.list_owner_devices().await.expect("devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].last_seen_at.as_deref(), Some("now"));
        assert_eq!(
            reopened
                .list_active_sessions(EXPIRES_AT_MS - 1_000)
                .await
                .expect("sessions")[0]
                .token_hash,
            "token-hash"
        );
        assert!(reopened
            .list_active_sessions(EXPIRES_AT_MS)
            .await
            .expect("expired sessions")
            .is_empty());
    }

    #[tokio::test]
    async fn automation_messages_use_the_most_recent_active_owner_device() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("older", "Older", "{}", "2026-09-01T00:00:00Z")
            .await
            .expect("older device");
        store
            .upsert_owner_device("newer", "Newer", "{}", "2026-09-02T00:00:00Z")
            .await
            .expect("newer device");
        assert_eq!(
            store
                .automation_message_device_id()
                .await
                .expect("automation device"),
            Some("newer".into())
        );
        store
            .revoke_owner_device("newer", "2026-09-02T01:00:00Z")
            .await
            .expect("revoke newer");
        assert_eq!(
            store
                .automation_message_device_id()
                .await
                .expect("fallback automation device"),
            Some("older".into())
        );
    }

    #[tokio::test]
    async fn owner_device_rename_persists_without_invalidating_session() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let store = Store::connect(&database_url).await.expect("store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        store
            .insert_session("token-hash", "device-1", "csrf-hash", 2_000, "now")
            .await
            .expect("session");

        assert!(store
            .rename_owner_device("device-1", "Phone")
            .await
            .expect("rename"));
        assert!(!store
            .rename_owner_device("missing", "Other")
            .await
            .expect("missing rename"));
        assert_eq!(
            store.list_owner_devices().await.expect("devices")[0].label,
            "Phone"
        );
        assert_eq!(
            store
                .list_active_sessions(1_000)
                .await
                .expect("sessions")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn forgotten_devices_are_hidden_but_keep_history_references() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .unwrap();
        store
            .upsert_owner_device("old-phone", "Phone", "{}", "now")
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE devices SET forgotten = 1 WHERE id = 'old-phone'")
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(!store.forget_revoked_device("old-phone").await.unwrap());
        store
            .revoke_owner_device("old-phone", "later")
            .await
            .unwrap();
        assert!(store.forget_revoked_device("old-phone").await.unwrap());
        assert!(!store.forget_revoked_device("old-phone").await.unwrap());
        assert!(!store.forget_revoked_device("missing").await.unwrap());
        assert!(store.list_owner_devices().await.unwrap().is_empty());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE id = 'old-phone'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn owner_device_revoke_invalidates_all_sessions() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let store = Store::connect(&database_url).await.expect("store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        store
            .insert_session("token-one", "device-1", "csrf-one", 2_000, "now")
            .await
            .expect("session one");
        store
            .insert_session("token-two", "device-1", "csrf-two", 2_000, "now")
            .await
            .expect("session two");

        assert!(store
            .revoke_owner_device("device-1", "revoked")
            .await
            .expect("revoke"));
        assert!(store
            .list_active_sessions(1_000)
            .await
            .expect("sessions")
            .is_empty());
        assert!(!store
            .rename_owner_device("device-1", "Renamed revoked device")
            .await
            .expect("rename revoked device"));
    }

    /// Queries are written and tested against the planner's defaults; sampled
    /// statistics turned indexed claim/recovery reads into scans. Opening
    /// removes ones an earlier build wrote and never writes new ones.
    #[tokio::test]
    async fn opening_the_store_leaves_no_planner_statistics() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        sqlx::migrate::Migrator::with_migrations(
            sqlx::migrate!()
                .iter()
                .filter(|m| m.version < 133)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        for id in 0..50 {
            sqlx::query("INSERT INTO conversations(id,created_at) VALUES (?, 'then')")
                .bind(format!("c{id}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("ANALYZE").execute(&pool).await.unwrap();
        pool.close().await;
        let statistics = |store: &Store| {
            let pool = store.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='sqlite_stat1'",
                )
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let store = Store::connect(&database_url).await.expect("store");
        assert_eq!(statistics(&store).await, 0);
        store.pool.close().await;
        let store = Store::connect(&database_url).await.expect("reopened store");
        assert_eq!(statistics(&store).await, 0);
    }

    #[tokio::test]
    async fn conversation_thread_survives_store_reopen() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let store = Store::connect(&database_url).await.expect("store");
        bind_project_fixture(
            &store,
            "conversation-1",
            AgentFamily::Codex,
            "thread-1",
            Some("session-1"),
            "now",
        )
        .await
        .expect("thread");
        drop(store);
        let reopened = Store::connect(&database_url).await.expect("reopen");
        assert_eq!(
            reopened
                .conversation_thread("conversation-1")
                .await
                .expect("lookup"),
            Some("thread-1".into())
        );
    }

    #[tokio::test]
    async fn dispatch_claim_is_atomic() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        let inserted = store
            .insert_message(
                "device-1",
                "client-1",
                "body",
                "hash",
                "conversation-1",
                "now",
            )
            .await
            .expect("message");
        let message_id = match inserted {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected inserted message"),
        };
        let (first, second) = tokio::join!(
            store.claim_message_for_dispatch(&message_id),
            store.claim_message_for_dispatch(&message_id)
        );
        assert_eq!(
            first.expect("first claim") as u8 + second.expect("second claim") as u8,
            1
        );
    }

    #[tokio::test]
    async fn turn_owner_is_deterministic_when_a_steer_reuses_the_turn_id() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        let first = store
            .insert_message(
                "device-1",
                "client-1",
                "original",
                "hash-1",
                "conversation-1",
                "2026-09-01T10:00:00.000Z",
            )
            .await
            .expect("original message");
        let first_id = match first {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected original message"),
        };
        let second = store
            .insert_message(
                "device-1",
                "client-2",
                "steered",
                "hash-2",
                "conversation-1",
                "2026-09-01T10:01:00.000Z",
            )
            .await
            .expect("steered message");
        let second_id = match second {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected steered message"),
        };
        store
            .update_message_delivery(&first_id, "streaming", Some("thread-1"), Some("turn-1"))
            .await
            .expect("original delivery");
        store
            .update_message_delivery(
                &second_id,
                "accepted_by_codex",
                Some("thread-1"),
                Some("turn-1"),
            )
            .await
            .expect("steered delivery");

        let owner = store
            .message_for_codex_turn("turn-1")
            .await
            .expect("turn owner lookup")
            .expect("turn owner");
        assert_eq!(owner.id, second_id);
        assert_eq!(
            store
                .message_by_id(&first_id)
                .await
                .expect("original lookup")
                .expect("original")
                .state,
            "streaming"
        );
        assert_eq!(
            store
                .complete_messages_for_codex_turn("turn-1")
                .await
                .expect("complete turn"),
            2
        );
        assert_eq!(
            store
                .message_by_id(&first_id)
                .await
                .expect("completed original lookup")
                .expect("completed original")
                .state,
            "completed"
        );
    }

    #[tokio::test]
    async fn client_message_lookup_preserves_completed_steer_for_replay() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        let inserted = store
            .insert_message(
                "device-1",
                "client-steer",
                "steer body",
                "hash-steer",
                "conversation-1",
                "2026-09-01T10:00:00.000Z",
            )
            .await
            .expect("message");
        let message_id = match inserted {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected inserted message"),
        };
        store
            .update_message_delivery(&message_id, "completed", Some("thread-1"), Some("turn-1"))
            .await
            .expect("completed delivery");

        let replay = store
            .message_by_device_and_client_message_id("device-1", "client-steer")
            .await
            .expect("lookup")
            .expect("stored replay");
        assert_eq!(replay.id, message_id);
        assert_eq!(replay.state, "completed");
        assert_eq!(replay.codex_turn_id.as_deref(), Some("turn-1"));
    }

    #[tokio::test]
    async fn safe_to_retry_is_requeued_once() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        let inserted = store
            .insert_message(
                "device-1",
                "client-1",
                "body",
                "hash",
                "conversation-1",
                "now",
            )
            .await
            .expect("message");
        let message_id = match inserted {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected inserted message"),
        };
        store
            .fail_message_delivery(&message_id, "safe_to_retry", "fixture refusal")
            .await
            .expect("mark safe to retry");
        assert_eq!(
            store
                .message_delivery_error(&message_id)
                .await
                .unwrap()
                .as_deref(),
            Some("fixture refusal")
        );
        assert!(store
            .fail_message_delivery(&message_id, "streaming", "invalid transition")
            .await
            .is_err());
        assert!(store
            .requeue_message_for_retry(&message_id)
            .await
            .expect("requeue"));
        assert!(!store
            .requeue_message_for_retry(&message_id)
            .await
            .expect("duplicate requeue"));
        assert_eq!(
            store
                .message_by_id(&message_id)
                .await
                .expect("lookup")
                .expect("message")
                .state,
            "accepted_by_wonder"
        );
        assert_eq!(
            store.message_delivery_error(&message_id).await.unwrap(),
            None
        );
        store
            .update_message_delivery(&message_id, "streaming", Some("thread-1"), Some("turn-1"))
            .await
            .expect("start turn");
        assert!(store
            .interrupt_message_if_active(&message_id)
            .await
            .expect("interrupt"));
        assert!(!store
            .complete_message_if_active(&message_id)
            .await
            .expect("ignore late completion"));
        assert_eq!(
            store
                .message_by_id(&message_id)
                .await
                .expect("lookup")
                .expect("message")
                .state,
            "interrupted"
        );
    }

    #[tokio::test]
    async fn message_attachment_refs_are_atomic_and_idempotent() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Browser", "{}", "now")
            .await
            .expect("device");
        store
            .upsert_conversation_file(
                "550e8400-e29b-41d4-a716-446655440000",
                "conversation-1",
                "attachment",
                "brief.txt",
                Some("text/plain"),
                Some(5),
                Some(&"a".repeat(64)),
                Some(".wonder/attachments/550e8400-e29b-41d4-a716-446655440000"),
                "available",
                None,
                None,
                None,
                "now",
            )
            .await
            .expect("file");
        let file_id = "550e8400-e29b-41d4-a716-446655440000".to_owned();
        let inserted = store
            .insert_message_with_attachments(
                "device-1",
                "client-attachment",
                "Review this file",
                "hash",
                "conversation-1",
                std::slice::from_ref(&file_id),
                "now",
            )
            .await
            .expect("message");
        let message_id = match inserted {
            MessageInsert::Inserted(message) => message.id,
            _ => panic!("expected inserted message"),
        };
        assert_eq!(
            store
                .attachment_ids_for_message(&message_id)
                .await
                .expect("refs"),
            vec![file_id.clone()]
        );
        assert_eq!(
            store
                .attachments_for_message(&message_id)
                .await
                .expect("files")[0]
                .name,
            "brief.txt"
        );
        assert!(matches!(
            store
                .insert_message_with_attachments(
                    "device-1",
                    "client-attachment",
                    "Review this file",
                    "hash",
                    "conversation-1",
                    std::slice::from_ref(&file_id),
                    "later",
                )
                .await
                .expect("replay"),
            MessageInsert::Existing(_)
        ));
        assert_eq!(
            store
                .insert_message_with_attachments(
                    "device-1",
                    "client-attachment",
                    "Review this file",
                    "hash",
                    "conversation-1",
                    &[],
                    "later",
                )
                .await
                .expect("conflict"),
            MessageInsert::Conflict
        );
        assert!(matches!(
            store
                .insert_message_with_attachments(
                    "device-1",
                    "client-missing-attachment",
                    "Review missing",
                    "hash-2",
                    "conversation-1",
                    &["550e8400-e29b-41d4-a716-446655440001".to_owned()],
                    "now",
                )
                .await
                .expect("missing attachment"),
            MessageInsert::Conflict
        ));
    }

    // Contract: removing a draft's attachment forgets only the owner's own
    // upload that no message carries; a sent file and Mac-side media stay.
    #[tokio::test]
    async fn only_unsent_uploads_can_be_removed() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-uploads", "Phone", "{}", "now")
            .await
            .expect("device");
        let conversation = "conversation-uploads";
        let [sent, draft, media] = [
            "7c9e6679-7425-40de-944b-e07fc1f90ae1",
            "7c9e6679-7425-40de-944b-e07fc1f90ae2",
            "7c9e6679-7425-40de-944b-e07fc1f90ae3",
        ];
        for (id, source) in [
            (sent, None),
            (draft, None),
            (media, Some("tool-media:t:i:0")),
        ] {
            store
                .upsert_conversation_file(
                    id,
                    conversation,
                    "attachment",
                    "sketch.png",
                    Some("image/png"),
                    Some(5),
                    Some(&"a".repeat(64)),
                    Some(&format!(".wonder/attachments/{id}")),
                    "available",
                    None,
                    None,
                    source,
                    "now",
                )
                .await
                .expect("file");
        }
        assert!(matches!(
            store
                .insert_message_with_attachments(
                    "device-uploads",
                    "client-sent-upload",
                    "Look",
                    "hash-upload",
                    conversation,
                    &[sent.to_owned()],
                    "now",
                )
                .await
                .expect("message"),
            MessageInsert::Inserted(_)
        ));
        let remove = |id: &'static str, conversation: &'static str| {
            let store = store.clone();
            async move { store.remove_unsent_upload(conversation, id).await.unwrap() }
        };
        assert_eq!(remove(sent, conversation).await, UploadRemoval::Referenced);
        assert_eq!(remove(media, conversation).await, UploadRemoval::Missing);
        assert_eq!(
            remove(draft, "another-conversation").await,
            UploadRemoval::Missing
        );
        assert!(
            matches!(remove(draft, conversation).await, UploadRemoval::Removed(file) if file.id == draft)
        );
        assert_eq!(remove(draft, conversation).await, UploadRemoval::Missing);
        // A send racing the removal can no longer pick the file up.
        assert_eq!(
            store
                .insert_message_with_attachments(
                    "device-uploads",
                    "client-removed-upload",
                    "Look again",
                    "hash-upload-2",
                    conversation,
                    &[draft.to_owned()],
                    "now",
                )
                .await
                .expect("refused"),
            MessageInsert::Conflict
        );
        let left = store.list_conversation_files(conversation).await.unwrap();
        assert_eq!(left.len(), 2);
    }

    #[tokio::test]
    async fn search_indexes_project_messages_assistant_text_and_files() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        seed_project_thread(
            &store,
            "researcher",
            "Research Project",
            "/tmp/wonder-researcher",
            None,
            None,
            "now",
        )
        .await
        .expect("project");
        seed_project_thread(
            &store,
            "archived-researcher",
            "Archived Sources Project",
            "/tmp/wonder-archived",
            None,
            None,
            "now",
        )
        .await
        .expect("archived project");
        store
            .set_project_conversation_wonder_archived("archived-researcher", true, "now")
            .await
            .expect("archive");
        seed_project_thread(
            &store,
            "conversation-search",
            "Launch notes",
            "/tmp/wonder-researcher",
            None,
            None,
            "now",
        )
        .await
        .expect("conversation");
        store
            .upsert_owner_device("device-1", "Search test", "{}", "now")
            .await
            .expect("device");
        store
            .insert_message(
                "device-1",
                "client-search",
                "Compare the launch sources",
                "hash-search",
                "conversation-search",
                "2026-09-01T10:00:00Z",
            )
            .await
            .expect("message");
        store
            .upsert_conversation_file(
                "file-search",
                "conversation-search",
                "attachment",
                "launch-sources.pdf",
                Some("application/pdf"),
                Some(12),
                Some(&"a".repeat(64)),
                Some(".wonder/attachments/file-search"),
                "available",
                None,
                None,
                None,
                "2026-09-01T10:01:00Z",
            )
            .await
            .expect("file");
        store
            .complete_assistant_message(
                "conversation-search",
                "thread-search",
                "turn-search",
                "item-search",
                "The launch sources are ready.",
                "2026-09-01T10:02:00Z",
            )
            .await
            .expect("assistant");

        let source_results = store.search("sources", 20).await.expect("search");
        assert!(source_results.iter().any(|result| result.kind == "message"));
        assert!(source_results
            .iter()
            .any(|result| result.kind == "assistant_message"));
        assert!(source_results.iter().any(|result| result.kind == "file"));
        assert_eq!(
            store
                .search("Research Project", 20)
                .await
                .expect("project search")[0]
                .kind,
            "conversation"
        );
        assert!(store
            .search("Archived Sources Project", 20)
            .await
            .expect("archived search")
            .is_empty());
        assert!(store.search("", 20).await.expect("empty search").is_empty());
    }

    #[tokio::test]
    async fn approval_resolution_is_single_use_and_retryable_after_response_failure() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .insert_pending_approval(
                "42",
                "item/commandExecution/requestApproval",
                r#"{"availableDecisions":["accept","decline"]}"#,
                "now",
            )
            .await
            .expect("approval");
        let first = store
            .begin_approval_resolution("42")
            .await
            .expect("begin")
            .expect("pending approval");
        assert_eq!(first.state, "resolving");
        assert!(store
            .begin_approval_resolution("42")
            .await
            .expect("second begin")
            .is_none());
        store.reset_approval_resolution("42").await.expect("reset");
        assert!(store
            .begin_approval_resolution("42")
            .await
            .expect("retry begin")
            .is_some());
        store
            .finish_approval_resolution("42", "accept", "later")
            .await
            .expect("finish");
        store
            .insert_pending_approval(
                "42",
                "item/commandExecution/requestApproval",
                "{}",
                "replayed",
            )
            .await
            .expect("replay");
        assert!(
            store
                .list_pending_approvals()
                .await
                .expect("approvals")
                .is_empty(),
            "a replay must not reopen an owner-resolved request"
        );

        store
            .insert_pending_approval(
                "runtime-2:42",
                "item/tool/call",
                r#"{"threadId":"thread-new","turnId":"turn-new","itemId":"item-new","tool":"wonder_computer_use"}"#,
                "newer",
            )
            .await
            .expect("reopened approval");
        let reopened = store
            .list_pending_approvals()
            .await
            .expect("pending approvals")
            .into_iter()
            .find(|approval| approval.approval_id == "runtime-2:42")
            .expect("reopened approval is pending");
        assert_eq!(reopened.method, "item/tool/call");
        assert_eq!(reopened.thread_id, "thread-new");
        assert_eq!(reopened.turn_id, "turn-new");
        assert_eq!(reopened.item_id, "item-new");
        assert_eq!(reopened.state, "pending");
        assert_ne!(reopened.action_nonce, first.action_nonce);
        store
            .begin_approval_resolution("runtime-2:42")
            .await
            .expect("begin reopened approval")
            .expect("reopened approval is resolvable");
        store
            .finish_approval_resolution("runtime-2:42", "respond", "newer-later")
            .await
            .expect("finish reopened approval");
        store
            .insert_pending_approval(
                "43",
                "item/fileChange/requestApproval",
                r#"{"availableDecisions":["accept","decline"]}"#,
                "now",
            )
            .await
            .expect("second approval");
        assert!(store
            .resolve_approval_by_server_request_id("43", "app_server_resolved", "later")
            .await
            .expect("server resolution"));
        assert!(store
            .begin_approval_resolution("42")
            .await
            .expect("resolved begin")
            .is_none());
    }

    #[tokio::test]
    async fn conversation_files_are_durable() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_conversation_file(
                "file-1",
                "default",
                "file_change",
                "main.rs",
                None,
                None,
                None,
                Some("src/main.rs"),
                "update",
                Some(4),
                Some(2),
                Some("item-1"),
                "now",
            )
            .await
            .expect("file");
        let file = store
            .list_conversation_files("default")
            .await
            .expect("files")
            .pop()
            .expect("file row");
        assert_eq!(file.relative_path.as_deref(), Some("src/main.rs"));
        assert_eq!(file.additions, Some(4));
        let attachment_hash = "a".repeat(64);
        store
            .upsert_conversation_file(
                "file-2",
                "default",
                "attachment",
                "note.txt",
                Some("text/plain"),
                Some(4),
                Some(&attachment_hash),
                Some(".wonder/attachments/file-2"),
                "available",
                None,
                None,
                None,
                "later",
            )
            .await
            .expect("second file");
        for (name, mime, now, expected) in [
            ("note.txt", Some("text/plain"), "replayed", "later"),
            ("renamed.txt", Some("text/plain"), "renamed", "renamed"),
            ("renamed.txt", None, "missing-mime", "missing-mime"),
            ("renamed.txt", None, "replayed-again", "missing-mime"),
            ("renamed.txt", Some("text/plain"), "restored", "restored"),
        ] {
            let updated_at = store
                .upsert_conversation_file(
                    "file-2",
                    "default",
                    "attachment",
                    name,
                    mime,
                    Some(4),
                    Some(&attachment_hash),
                    Some(".wonder/attachments/file-2"),
                    "available",
                    None,
                    None,
                    None,
                    now,
                )
                .await
                .unwrap();
            assert_eq!(updated_at, expected);
            let saved = store
                .conversation_files_by_ids("default", &["file-2".into()])
                .await
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(saved.updated_at, expected);
        }
        store
            .upsert_conversation_file(
                "foreign",
                "another-conversation",
                "attachment",
                "other.txt",
                None,
                None,
                None,
                None,
                "available",
                None,
                None,
                None,
                "later",
            )
            .await
            .expect("foreign file");
        assert!(store
            .conversation_files_by_ids("default", &[])
            .await
            .unwrap()
            .is_empty());
        let selected = store
            .conversation_files_by_ids(
                "default",
                &[
                    "file-1".into(),
                    "foreign".into(),
                    "missing".into(),
                    "file-2".into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected.iter().find(|row| row.id == "file-1").unwrap(),
            &file
        );
        let attachment = selected.iter().find(|row| row.id == "file-2").unwrap();
        assert_eq!(attachment.kind, "attachment");
        assert_eq!(attachment.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(attachment.sha256.as_deref(), Some(attachment_hash.as_str()));
        assert_eq!(attachment.state, "available");
        let mut bounded_ids = vec!["file-1".to_owned(), "file-2".to_owned()];
        for index in 0..30 {
            let id = format!("batch-{index}");
            store
                .upsert_conversation_file(
                    &id,
                    "default",
                    "attachment",
                    &id,
                    None,
                    None,
                    None,
                    None,
                    "available",
                    None,
                    None,
                    None,
                    "later",
                )
                .await
                .unwrap();
            bounded_ids.push(id);
        }
        assert_eq!(
            store
                .conversation_files_by_ids("default", &bounded_ids)
                .await
                .unwrap()
                .len(),
            32
        );
        bounded_ids.push("missing".into());
        assert!(
            matches!(store.conversation_files_by_ids("default", &bounded_ids).await,
            Err(sqlx::Error::Protocol(message)) if message == "too_many_file_ids")
        );
    }

    #[tokio::test]
    async fn workspace_artifact_metadata_round_trips_without_a_new_schema() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        let sha256 = "a".repeat(64);
        store
            .upsert_conversation_file(
                "artifact-1",
                "conversation-1",
                "artifact",
                "report.pdf",
                Some("application/pdf"),
                Some(12),
                Some(&sha256),
                Some("reports/report.pdf"),
                "available",
                None,
                None,
                Some("item-1"),
                "now",
            )
            .await
            .expect("artifact metadata");
        let artifact = store
            .list_conversation_files("conversation-1")
            .await
            .expect("artifact list")
            .pop()
            .expect("artifact row");
        assert_eq!(artifact.kind, "artifact");
        assert_eq!(artifact.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(artifact.byte_size, Some(12));
        assert_eq!(artifact.sha256.as_deref(), Some(sha256.as_str()));
        assert_eq!(
            artifact.relative_path.as_deref(),
            Some("reports/report.pdf")
        );
        assert_eq!(artifact.source_id.as_deref(), Some("item-1"));
    }

    /// The Outputs index spans the Project's threads and no other Project,
    /// lists the agent's files without the owner's uploads by default,
    /// filters by type and name, pages pinned-first without repeats, and
    /// links a revision to the output it replaces.
    #[tokio::test]
    async fn project_outputs_index_the_threads_agent_files() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        ))
        .await
        .expect("store");
        seed_project_thread(
            &store,
            "chat-a",
            "Weekly report",
            "/tmp/outputs-a",
            None,
            None,
            "t0",
        )
        .await
        .expect("first thread");
        store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "chat-b",
                project_id: "project-chat-a",
                family: AgentFamily::Codex,
                provider_store: "fixture-store",
                native_session_id: None,
                cwd: "/tmp/outputs-a",
                roots_revision: 1,
                title: "Landing page",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "t0",
            })
            .await
            .expect("second thread");
        seed_project_thread(
            &store,
            "elsewhere",
            "Other",
            "/tmp/outputs-b",
            None,
            None,
            "t0",
        )
        .await
        .expect("other project");
        let file = |id: &'static str,
                    chat: &'static str,
                    name: &'static str,
                    mime: &'static str,
                    source: Option<&'static str>,
                    at: &'static str| {
            let store = &store;
            async move {
                store
                    .upsert_conversation_file(
                        id,
                        chat,
                        "attachment",
                        name,
                        Some(mime),
                        Some(3),
                        Some("abc"),
                        None,
                        "available",
                        None,
                        None,
                        source,
                        at,
                    )
                    .await
                    .expect("file");
            }
        };
        file(
            "report",
            "chat-a",
            "Weekly Report.pdf",
            "application/pdf",
            Some("tool-media:t:i:0"),
            "t1",
        )
        .await;
        file(
            "chart",
            "chat-a",
            "chart.png",
            "image/png",
            Some("tool-media:t:i:1"),
            "t2",
        )
        .await;
        file(
            "page-1",
            "chat-b",
            "Landing.html",
            "text/html",
            Some("wonder-visual:m:c1"),
            "t3",
        )
        .await;
        file(
            "page-2",
            "chat-b",
            "Landing.html",
            "text/html",
            Some("wonder-visual:m:c2"),
            "t4",
        )
        .await;
        file(
            "visual",
            "chat-b",
            "Plan.json",
            "application/vnd.wonder.ui+json",
            Some("wonder-visual:m:c3"),
            "t5",
        )
        .await;
        file(
            "upload",
            "chat-a",
            "brief.pdf",
            "application/pdf",
            None,
            "t6",
        )
        .await;
        file(
            "sent-picture",
            "chat-a",
            "me.png",
            "image/png",
            Some("user-media:t:i:0"),
            "t7",
        )
        .await;
        file(
            "foreign",
            "elsewhere",
            "secret.pdf",
            "application/pdf",
            Some("tool-media:t:i:0"),
            "t8",
        )
        .await;
        assert!(store
            .set_conversation_file_replaces("chat-b", "page-2", "page-1")
            .await
            .unwrap());
        assert!(
            !store
                .set_conversation_file_replaces("chat-b", "page-2", "report")
                .await
                .unwrap(),
            "a revision replaces a file of its own thread"
        );

        let ids = |outputs: Vec<StoredProjectOutput>| {
            outputs
                .into_iter()
                .map(|output| output.file.id)
                .collect::<Vec<_>>()
        };
        let query = |file_type, name| ProjectOutputQuery {
            origin: ProjectOutputOrigin::Agent,
            file_type,
            name,
            after: None,
            limit: 50,
        };
        let all = store
            .project_outputs("project-chat-a", &query(ProjectOutputType::All, None))
            .await
            .unwrap();
        assert_eq!(all[0].conversation_title, "Landing page");
        assert_eq!(all[1].replaces_file_id.as_deref(), Some("page-1"));
        assert_eq!(
            ids(all),
            ["visual", "page-2", "page-1", "chart", "report"],
            "agent files only, newest first, from both threads"
        );
        for (file_type, expected) in [
            (ProjectOutputType::Pages, vec!["visual", "page-2", "page-1"]),
            (ProjectOutputType::Images, vec!["chart"]),
            (ProjectOutputType::Documents, vec!["report"]),
        ] {
            assert_eq!(
                ids(store
                    .project_outputs("project-chat-a", &query(file_type, None))
                    .await
                    .unwrap()),
                expected,
                "{file_type:?}"
            );
        }
        assert_eq!(
            ids(store
                .project_outputs(
                    "project-chat-a",
                    &query(ProjectOutputType::All, Some("weekly"))
                )
                .await
                .unwrap()),
            ["report"]
        );
        assert!(
            store
                .project_outputs("project-chat-a", &query(ProjectOutputType::All, Some("%")))
                .await
                .unwrap()
                .is_empty(),
            "a search is literal text, not a pattern"
        );
        let owner = ProjectOutputQuery {
            origin: ProjectOutputOrigin::Owner,
            ..query(ProjectOutputType::All, None)
        };
        assert_eq!(
            ids(store
                .project_outputs("project-chat-a", &owner)
                .await
                .unwrap()),
            ["sent-picture", "upload"]
        );

        assert!(store
            .set_project_output_pinned("project-chat-a", "report", true, "t9")
            .await
            .unwrap());
        assert!(!store
            .set_project_output_pinned("project-chat-a", "foreign", true, "t9")
            .await
            .unwrap());
        let mut paged = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .project_outputs(
                    "project-chat-a",
                    &ProjectOutputQuery {
                        after: after.clone(),
                        limit: 2,
                        ..query(ProjectOutputType::All, None)
                    },
                )
                .await
                .unwrap();
            let Some(last) = page.last() else { break };
            after = Some(ProjectOutputCursor {
                pinned: last.pinned,
                updated_at: last.file.updated_at.clone(),
                id: last.file.id.clone(),
            });
            paged.extend(page);
        }
        assert!(paged[0].pinned);
        assert_eq!(
            ids(paged),
            ["report", "visual", "page-2", "page-1", "chart"]
        );

        assert!(store
            .set_project_output_pinned("project-chat-a", "report", false, "t9")
            .await
            .unwrap());
        assert!(
            !store
                .project_outputs("project-chat-a", &query(ProjectOutputType::Documents, None))
                .await
                .unwrap()[0]
                .pinned
        );
    }

    #[tokio::test]
    async fn assistant_projection_is_idempotent_and_survives_reopen() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let store = Store::connect(&database_url).await.expect("store");
        let first = store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "hello",
                "notification-1",
                "2026-09-01T10:00:00Z",
            )
            .await
            .expect("first delta");
        let replay = store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "hello",
                "notification-1",
                "2026-09-01T10:00:01Z",
            )
            .await
            .expect("replayed delta");
        let distinct = store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "hello",
                "notification-2",
                "2026-09-01T10:00:01.500Z",
            )
            .await
            .expect("distinct delta");
        assert_eq!(first.text, "hello");
        assert_eq!(replay.text, "hello");
        assert_eq!(distinct.text, "hellohello");
        store
            .complete_assistant_message(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "hello world",
                "2026-09-01T10:00:02Z",
            )
            .await
            .expect("completion")
            .expect("active completion");
        assert!(store
            .complete_assistant_message(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "late completion must not overwrite fallback text",
                "2026-09-01T10:00:02.500Z",
            )
            .await
            .expect("late completion")
            .is_none());
        let late_replay = store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "corrupting replay",
                "notification-after-completion",
                "2026-09-01T10:00:03Z",
            )
            .await
            .expect("late replay");
        // App Server completion text is authoritative. A repeated or late
        // chunk must never mutate the completed message, even when no
        // sequence identity was available for the chunk.
        assert_eq!(late_replay.text, "hello world");
        drop(store);

        let reopened = Store::connect(&database_url).await.expect("reopen");
        let messages = reopened
            .assistant_messages_for_conversation("conversation-1")
            .await
            .expect("assistant messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello world");
        assert_eq!(messages[0].state, "completed");
    }

    #[tokio::test]
    async fn completing_codex_turn_completes_active_assistant_rows_with_context() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-1",
                "hello",
                "notification-1",
                "2026-09-01T10:00:00Z",
            )
            .await
            .expect("first delta");
        store
            .upsert_assistant_delta(
                "conversation-1",
                "thread-1",
                "turn-1",
                "item-2",
                "world",
                "notification-2",
                "2026-09-01T10:00:01Z",
            )
            .await
            .expect("second delta");
        store
            .upsert_assistant_delta(
                "conversation-2",
                "thread-2",
                "turn-1",
                "item-3",
                "other",
                "notification-3",
                "2026-09-01T10:00:02Z",
            )
            .await
            .expect("other turn delta");

        let completed = store
            .complete_assistant_messages_for_codex_thread_and_turn(
                "thread-1",
                "turn-1",
                "2026-09-01T10:00:03Z",
            )
            .await
            .expect("complete assistant turn");
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].conversation_id, "conversation-1");
        assert_eq!(completed[0].codex_thread_id, "thread-1");
        assert_eq!(completed[0].codex_turn_id, "turn-1");
        assert_eq!(completed[0].item_id, "item-1");
        assert_eq!(completed[0].text, "hello");
        assert_eq!(completed[0].state, "completed");
        assert_eq!(completed[0].updated_at, "2026-09-01T10:00:03Z");
        assert!(store
            .complete_assistant_messages_for_codex_thread_and_turn(
                "thread-1",
                "turn-1",
                "2026-09-01T10:00:04Z",
            )
            .await
            .expect("ignore completed assistant turn")
            .is_empty());
        assert!(store
            .assistant_messages_for_conversation("conversation-2")
            .await
            .expect("other turn lookup")
            .iter()
            .all(|message| message.state == "streaming"));
        assert!(store
            .complete_assistant_messages_for_codex_thread_and_turn(
                "thread-1",
                "turn-1",
                "2026-09-01T10:00:04Z",
            )
            .await
            .expect("ignore completed assistant thread")
            .is_empty());
    }

    #[tokio::test]
    async fn app_server_notification_receipt_is_durable_and_single_use() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        assert!(store
            .claim_app_server_notification("notification-1", "now")
            .await
            .expect("first receipt"));
        assert!(!store
            .claim_app_server_notification("notification-1", "later")
            .await
            .expect("replayed receipt"));
    }

    #[tokio::test]
    async fn app_server_notification_receipt_can_be_released_for_retry() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        assert!(store
            .claim_app_server_notification("notification-1", "now")
            .await
            .expect("first receipt"));
        store
            .release_app_server_notification("notification-1")
            .await
            .expect("release receipt");
        assert!(store
            .claim_app_server_notification("notification-1", "retry")
            .await
            .expect("retry receipt"));
    }

    #[tokio::test]
    async fn mapped_assistant_projection_failure_preserves_full_notification_for_retry() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        let notification = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "item-1"},
                "delta": {"text": "retry me"}
            }
        });
        let params = notification.get("params").expect("params");
        assert!(store
            .enqueue_pending_app_server_notification(
                "pending-1",
                None,
                "item/agentMessage/delta",
                Some("thread-1"),
                Some("turn-1"),
                Some("item-1"),
                &notification.to_string(),
                &params.to_string(),
                "now",
            )
            .await
            .expect("queue projection failure"));

        let pending = store
            .pending_app_server_notifications_for_turn("turn-1")
            .await
            .expect("pending notification");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].notification_json, notification.to_string());
        assert_eq!(pending[0].identity_key, None);
    }

    #[tokio::test]
    async fn failed_retry_enqueue_leaves_claimed_notification_receipt_intact() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        assert!(store
            .claim_app_server_notification("notification-1", "now")
            .await
            .expect("claim receipt"));
        sqlx::query("DROP TABLE pending_app_server_notifications")
            .execute(&store.pool)
            .await
            .expect("simulate retry enqueue failure");
        assert!(store
            .enqueue_pending_app_server_notification(
                "pending-1",
                Some("notification-1"),
                "item/agentMessage/delta",
                Some("thread-1"),
                Some("turn-1"),
                Some("item-1"),
                r#"{"method":"item/agentMessage/delta"}"#,
                r#"{"turnId":"turn-1"}"#,
                "later",
            )
            .await
            .is_err());
        assert!(!store
            .claim_app_server_notification("notification-1", "retry")
            .await
            .expect("receipt remains claimed"));
    }

    #[tokio::test]
    async fn message_idempotency_is_bound_to_its_conversation() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("device-1", "Chrome", "{}", "now")
            .await
            .expect("device");
        let first = store
            .insert_message(
                "device-1",
                "client-1",
                "body",
                "hash",
                "conversation-1",
                "now",
            )
            .await
            .expect("first message");
        assert!(matches!(first, MessageInsert::Inserted(_)));
        let other_route = store
            .insert_message(
                "device-1",
                "client-1",
                "body",
                "hash",
                "conversation-2",
                "later",
            )
            .await
            .expect("other route replay");
        assert_eq!(other_route, MessageInsert::Conflict);
    }

    #[tokio::test]
    async fn workspace_reset_preserves_one_device_and_removes_user_data() {
        let store = Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("in-memory store");
        store
            .upsert_owner_device("keep", "Current", "{}", "now")
            .await
            .expect("keep device");
        store
            .upsert_owner_device("revoke", "Old", "{}", "now")
            .await
            .expect("old device");
        seed_project_thread(
            &store,
            "project-1",
            "Project One",
            "/tmp/wonder-project-1",
            None,
            None,
            "now",
        )
        .await
        .expect("project");
        let parent_conversation = "project-1";
        store
            .bind_project_runtime(
                parent_conversation,
                AgentFamily::Codex,
                "fixture-store",
                "reset-parent-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        store
            .register_subagent_ownership(
                "reset-child-conversation",
                parent_conversation,
                "reset-child-thread",
                "reset-parent-thread",
                Some("Reset child"),
                None,
                None,
                r#"{"subAgent":{"thread_spawn":{"parent_thread_id":"reset-parent-thread","depth":1}}}"#,
                Some("reset-runtime"),
                Some(true),
                "idle",
                None,
                "now",
            )
            .await
            .unwrap();
        let client = mcp::Client {
            id: "reset-client".into(),
            name: "Outside agent".into(),
            redirect_uris: vec!["http://localhost/callback".into()],
            created_at_ms: 0,
            revoked_at_ms: None,
        };
        assert!(store.mcp_register_client(&client).await.unwrap());
        let grant = mcp::Grant {
            id: "reset-grant".into(),
            client_id: client.id.clone(),
            client_name: client.name.clone(),
            audience: "https://mac.ts.net/mcp".into(),
            scopes: "read".into(),
            permission_mode: "read_only".into(),
            expires_at_ms: 500,
        };
        let tokens = mcp::TokenHashes {
            access: "reset-access-hash",
            refresh: "reset-refresh-hash",
            access_expires_at_ms: 100,
        };
        assert!(store.mcp_issue_grant(&grant, &tokens).await.unwrap());
        let code = mcp::AuthorizationCode {
            hash: "reset-code-hash".into(),
            client_id: client.id.clone(),
            redirect_uri: client.redirect_uris[0].clone(),
            code_challenge: "challenge".into(),
            audience: grant.audience.clone(),
            scopes: grant.scopes.clone(),
            permission_mode: grant.permission_mode.clone(),
            expires_at_ms: 60,
        };
        store
            .mcp_pairing_code("consumed-pair-hash", 50)
            .await
            .unwrap();
        assert!(store
            .mcp_approve("consumed-pair-hash", &code, 1)
            .await
            .unwrap());
        store
            .mcp_pairing_code("pending-pair-hash", 50)
            .await
            .unwrap();

        // A rejected reset must leave both the workspace and MCP access intact.
        assert!(matches!(
            store.reset_workspace("missing-owner", "reset").await,
            Err(sqlx::Error::RowNotFound)
        ));
        assert!(store
            .mcp_access(tokens.access, &grant.audience, 2)
            .await
            .unwrap()
            .is_some());
        let auth_counts = [
            ("SELECT COUNT(*) FROM mcp_clients", 1),
            ("SELECT COUNT(*) FROM mcp_grants", 1),
            ("SELECT COUNT(*) FROM mcp_tokens", 2),
            ("SELECT COUNT(*) FROM mcp_authorization_codes", 1),
            ("SELECT COUNT(*) FROM mcp_pairing_codes", 1),
        ];
        for (query, expected) in auth_counts {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query))
                    .fetch_one(&store.pool)
                    .await
                    .unwrap(),
                expected,
                "MCP state before reset: {query}"
            );
        }
        let reset = store.reset_workspace("keep", "reset").await.expect("reset");
        assert!(store
            .mcp_access(tokens.access, &grant.audience, 2)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .mcp_refresh(
                tokens.refresh,
                &client.id,
                &grant.audience,
                2,
                &tokens,
                None
            )
            .await
            .unwrap()
            .is_none());
        assert!(store.mcp_take_code(&code.hash).await.unwrap().is_none());
        for (query, _) in auth_counts {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query))
                    .fetch_one(&store.pool)
                    .await
                    .unwrap(),
                0,
                "MCP state after reset: {query}"
            );
        }
        // Even re-registering the same ID cannot revive a pre-reset Mac code.
        assert!(store.mcp_register_client(&client).await.unwrap());
        assert!(!store
            .mcp_approve("pending-pair-hash", &code, 2)
            .await
            .unwrap());
        assert_eq!(reset.revoked_device_ids, vec!["revoke"]);
        assert_eq!(store.list_projects().await.expect("projects").len(), 0);
        assert!(store
            .subagent_ownership_for_thread("reset-child-thread")
            .await
            .expect("ownership lookup")
            .is_none());
        assert!(
            sqlx::query("SELECT 1 FROM conversations WHERE id='reset-child-conversation'")
                .fetch_optional(&store.pool)
                .await
                .expect("child metadata lookup")
                .is_none()
        );
        assert_eq!(store.list_owner_devices().await.expect("devices").len(), 2);
        let devices = store.list_owner_devices().await.expect("devices");
        assert!(devices
            .iter()
            .any(|device| device.id == "keep" && device.revoked_at.is_none()));
        assert!(devices
            .iter()
            .any(|device| device.id == "revoke" && device.revoked_at.as_deref() == Some("reset")));
    }

    // Turn completion reads, then writes. It reserves the writer before
    // reading, so a write another connection commits while it waits cannot
    // fail it; a deferred read would fail to upgrade on its stale snapshot.
    #[tokio::test]
    async fn turn_completion_survives_a_write_committed_while_it_waits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("immediate.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("INSERT INTO assistant_messages (id, conversation_id, codex_thread_id, codex_turn_id, item_id, text, state, created_at, updated_at) VALUES ('assistant:turn:item', 'chat', 'thread', 'turn', 'item', 'partial', 'streaming', 'now', 'now')")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut writer = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO action_nonces (nonce, used_at) VALUES ('other-write', '1')")
            .execute(&mut *writer)
            .await
            .unwrap();
        let completing = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .complete_assistant_message("chat", "thread", "turn", "item", "done", "later")
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        let completed = completing.await.unwrap().unwrap().unwrap();
        assert_eq!(completed.state, "completed");
        assert_eq!(completed.text, "done");
    }

    // The lookups made for each agent notification seek an index rather than
    // scanning their table; the thread's held notifications also come back in
    // index order instead of being sorted (one turn's few replies may be).
    #[tokio::test]
    async fn per_notification_lookups_use_indexes() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        for (statement, index_ordered) in [
            ("SELECT id FROM messages WHERE codex_thread_id = 't' AND codex_turn_id = 'u' ORDER BY created_at DESC, rowid DESC LIMIT 1", false),
            ("SELECT id FROM assistant_messages WHERE codex_thread_id = 't' AND codex_turn_id = 'u' AND state IN ('accepted_by_codex', 'streaming') ORDER BY created_at ASC, id ASC", false),
            ("SELECT id FROM assistant_messages WHERE codex_turn_id = 'u' AND state IN ('accepted_by_codex', 'streaming') ORDER BY created_at ASC, id ASC", false),
            ("SELECT id FROM conversations WHERE codex_thread_id = 't' LIMIT 1", true),
            ("SELECT id FROM pending_app_server_notifications WHERE thread_id = 't' ORDER BY received_at ASC, rowid ASC", true),
            ("SELECT token_hash FROM sessions WHERE expires_at_ms > 0", true),
        ] {
            let plan: Vec<String> =
                sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {statement}")))
                    .fetch_all(&store.pool)
                    .await
                    .unwrap()
                    .iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect();
            assert!(
                plan.iter().all(|step| step.starts_with("SEARCH ")
                    || (!index_ordered && step.starts_with("USE TEMP B-TREE"))),
                "{statement}: {plan:?}"
            );
        }
    }
}
