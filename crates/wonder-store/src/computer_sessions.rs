use super::*;

/// A computer session's `conversation_id` when the owner views the Mac before
/// any conversation exists. It is only a binding label: no conversation,
/// Project or notification is ever looked up by this ID.
pub const HOST_VIEW_CONVERSATION_ID: &str = "wonder-host-view";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputerSessionState {
    Preparing,
    AwaitingSource,
    Live,
    Paused,
    Stale,
    Ended,
    Failed,
    Unavailable,
}

impl ComputerSessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::AwaitingSource => "awaitingSource",
            Self::Live => "live",
            Self::Paused => "paused",
            Self::Stale => "stale",
            Self::Ended => "ended",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }
}

impl TryFrom<&str> for ComputerSessionState {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Ok(match value {
            "preparing" => Self::Preparing,
            "awaitingSource" => Self::AwaitingSource,
            "live" => Self::Live,
            "paused" => Self::Paused,
            "stale" => Self::Stale,
            "ended" => Self::Ended,
            "failed" => Self::Failed,
            "unavailable" => Self::Unavailable,
            _ => return Err(()),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputerSessionCreate {
    pub id: String,
    pub client_request_id: String,
    pub owner_device_id: String,
    pub host_installation_id: String,
    pub conversation_id: String,
    pub generation: u64,
    pub state: ComputerSessionState,
    pub video_quality: String,
    pub video_max_fps: u16,
    pub source_id: Option<String>,
    pub source_name: Option<String>,
    pub source_kind: Option<String>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_scale: Option<f64>,
    pub crop_json: Option<String>,
    pub geometry_revision: u64,
    pub failure_reason: Option<String>,
    pub now: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredComputerSession {
    pub id: String,
    pub client_request_id: String,
    pub owner_device_id: String,
    pub host_installation_id: String,
    pub conversation_id: String,
    pub generation: u64,
    pub state: String,
    pub video_quality: String,
    pub video_max_fps: u16,
    pub source_id: Option<String>,
    pub source_name: Option<String>,
    pub source_kind: Option<String>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_scale: Option<f64>,
    pub crop_json: Option<String>,
    pub geometry_revision: u64,
    pub failure_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_state_at: String,
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputerControlLeaseCreate {
    pub id: String,
    pub client_request_id: String,
    pub session_id: String,
    pub owner_device_id: String,
    pub host_installation_id: String,
    pub conversation_id: String,
    pub session_generation: u64,
    pub source_id: Option<String>,
    pub geometry_revision: u64,
    pub now: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredComputerControlLease {
    pub id: String,
    pub client_request_id: String,
    pub session_id: String,
    pub owner_device_id: String,
    pub host_installation_id: String,
    pub conversation_id: String,
    pub session_generation: u64,
    pub source_id: Option<String>,
    pub geometry_revision: u64,
    pub status: String,
    pub last_sequence: u64,
    pub acquired_at: String,
    pub updated_at: String,
    pub expires_at: String,
    pub released_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComputerLeaseAcquireResult {
    Granted(StoredComputerControlLease),
    Existing(StoredComputerControlLease),
    Busy(StoredComputerControlLease),
    Conflict,
}

fn stored(row: &sqlx::sqlite::SqliteRow) -> StoredComputerSession {
    StoredComputerSession {
        id: row.get("id"),
        client_request_id: row.get("client_request_id"),
        owner_device_id: row.get("owner_device_id"),
        host_installation_id: row.get("host_installation_id"),
        conversation_id: row.get("conversation_id"),
        generation: row.get::<i64, _>("generation") as u64,
        state: row.get("state"),
        video_quality: row.get("video_quality"),
        video_max_fps: row.get::<i64, _>("video_max_fps") as u16,
        source_id: row.get("source_id"),
        source_name: row.get("source_name"),
        source_kind: row.get("source_kind"),
        source_width: row.get::<Option<i64>, _>("source_width").map(|v| v as u32),
        source_height: row.get::<Option<i64>, _>("source_height").map(|v| v as u32),
        source_scale: row.get("source_scale"),
        crop_json: row.get("crop_json"),
        geometry_revision: row.get::<i64, _>("geometry_revision") as u64,
        failure_reason: row.get("failure_reason"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_state_at: row.get("last_state_at"),
        ended_at: row.get("ended_at"),
    }
}

fn stored_lease(row: &sqlx::sqlite::SqliteRow) -> StoredComputerControlLease {
    StoredComputerControlLease {
        id: row.get("id"),
        client_request_id: row.get("client_request_id"),
        session_id: row.get("session_id"),
        owner_device_id: row.get("owner_device_id"),
        host_installation_id: row.get("host_installation_id"),
        conversation_id: row.get("conversation_id"),
        session_generation: row.get::<i64, _>("session_generation") as u64,
        source_id: row.get("source_id"),
        geometry_revision: row.get::<i64, _>("geometry_revision") as u64,
        status: row.get("status"),
        last_sequence: row.get::<i64, _>("last_sequence") as u64,
        acquired_at: row.get("acquired_at"),
        updated_at: row.get("updated_at"),
        expires_at: row.get("expires_at"),
        released_at: row.get("released_at"),
    }
}

impl Store {
    async fn computer_session_by_id_with<'a, E>(
        executor: E,
        id: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Sqlite>,
    {
        let row = sqlx::query("SELECT id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, COALESCE(video_quality_v2, video_quality) AS video_quality, video_max_fps, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, ended_at FROM computer_sessions WHERE id=?")
            .bind(id)
            .fetch_optional(executor)
            .await?;
        Ok(row.as_ref().map(stored))
    }

    async fn lease_by_id_with<'a, E>(
        executor: E,
        id: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Sqlite>,
    {
        let row = sqlx::query("SELECT id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at, released_at FROM computer_control_leases WHERE id=?")
            .bind(id)
            .fetch_optional(executor)
            .await?;
        Ok(row.as_ref().map(stored_lease))
    }

    pub async fn computer_control_lease(
        &self,
        id: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error> {
        Self::lease_by_id_with(&self.pool, id).await
    }

    pub async fn computer_control_lease_by_request(
        &self,
        owner_device_id: &str,
        client_request_id: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error> {
        let row = sqlx::query("SELECT id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at, released_at FROM computer_control_leases WHERE owner_device_id=? AND client_request_id=?")
            .bind(owner_device_id)
            .bind(client_request_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(stored_lease))
    }

    /// The source-change preflight must compare the caller's lease before
    /// moving native capture. The rebind transaction repeats this check at commit.
    pub async fn active_computer_control_lease_for_session(
        &self,
        session_id: &str,
        now: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error> {
        let row = sqlx::query("SELECT id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at, released_at FROM computer_control_leases WHERE session_id=? AND status='active' AND expires_at>?")
            .bind(session_id).bind(now).fetch_optional(&self.pool).await?;
        Ok(row.as_ref().map(stored_lease))
    }

    pub async fn acquire_computer_control_lease(
        &self,
        request: &ComputerControlLeaseCreate,
    ) -> Result<ComputerLeaseAcquireResult, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("UPDATE computer_control_leases SET status='expired', updated_at=?, released_at=? WHERE status='active' AND expires_at<=?")
            .bind(&request.now)
            .bind(&request.now)
            .bind(&request.now)
            .execute(&mut *transaction)
            .await?;

        if let Some(row) = sqlx::query("SELECT id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at, released_at FROM computer_control_leases WHERE owner_device_id=? AND client_request_id=?")
            .bind(&request.owner_device_id)
            .bind(&request.client_request_id)
            .fetch_optional(&mut *transaction)
            .await?
        {
            let lease = stored_lease(&row);
            transaction.commit().await?;
            return Ok(if lease_matches_create(&lease, request) {
                ComputerLeaseAcquireResult::Existing(lease)
            } else {
                ComputerLeaseAcquireResult::Conflict
            });
        }

        let session_is_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM computer_sessions WHERE id=? AND owner_device_id=? AND host_installation_id=? AND conversation_id=? AND generation=? AND state='live' AND geometry_revision=? AND source_id IS ?)")
            .bind(&request.session_id)
            .bind(&request.owner_device_id)
            .bind(&request.host_installation_id)
            .bind(&request.conversation_id)
            .bind(request.session_generation as i64)
            .bind(request.geometry_revision as i64)
            .bind(&request.source_id)
            .fetch_one(&mut *transaction)
            .await?;
        if !session_is_live {
            return Err(sqlx::Error::Protocol("computer session is stale".into()));
        }

        if let Some(row) = sqlx::query("SELECT id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at, released_at FROM computer_control_leases WHERE host_installation_id=? AND status='active' LIMIT 1")
            .bind(&request.host_installation_id)
            .fetch_optional(&mut *transaction)
            .await?
        {
            transaction.commit().await?;
            return Ok(ComputerLeaseAcquireResult::Busy(stored_lease(&row)));
        }

        sqlx::query("INSERT INTO computer_control_leases (id, client_request_id, session_id, owner_device_id, host_installation_id, conversation_id, session_generation, source_id, geometry_revision, status, last_sequence, acquired_at, updated_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', 0, ?, ?, ?)")
            .bind(&request.id)
            .bind(&request.client_request_id)
            .bind(&request.session_id)
            .bind(&request.owner_device_id)
            .bind(&request.host_installation_id)
            .bind(&request.conversation_id)
            .bind(request.session_generation as i64)
            .bind(&request.source_id)
            .bind(request.geometry_revision as i64)
            .bind(&request.now)
            .bind(&request.now)
            .bind(&request.expires_at)
            .execute(&mut *transaction)
            .await?;
        let lease = Self::lease_by_id_with(&mut *transaction, &request.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        transaction.commit().await?;
        Ok(ComputerLeaseAcquireResult::Granted(lease))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn heartbeat_computer_control_lease(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        host_installation_id: &str,
        session_id: &str,
        conversation_id: &str,
        session_generation: u64,
        source_id: Option<&str>,
        geometry_revision: u64,
        now: &str,
        expires_at: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error> {
        let lease = self.computer_control_lease(lease_id).await?;
        let Some(lease) = lease else { return Ok(None) };
        if !lease_binding_matches(
            &lease,
            owner_device_id,
            host_installation_id,
            session_id,
            conversation_id,
            session_generation,
            source_id,
            geometry_revision,
        ) || lease.status != "active"
            || lease.expires_at.as_str() <= now
        {
            return Ok(None);
        }
        let result = sqlx::query("UPDATE computer_control_leases SET updated_at=?, expires_at=? WHERE id=? AND status='active' AND updated_at=? AND EXISTS(SELECT 1 FROM computer_sessions WHERE id=? AND state='live' AND generation=? AND geometry_revision=? AND source_id IS ?)")
            .bind(now)
            .bind(expires_at)
            .bind(lease_id)
            .bind(&lease.updated_at)
            .bind(session_id)
            .bind(session_generation as i64)
            .bind(geometry_revision as i64)
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            return Ok(None);
        }
        self.computer_control_lease(lease_id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn validate_computer_input_lease(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        host_installation_id: &str,
        session_id: &str,
        conversation_id: &str,
        session_generation: u64,
        source_id: Option<&str>,
        geometry_revision: u64,
        now: &str,
    ) -> Result<StoredComputerControlLease, &'static str> {
        let lease = self
            .computer_control_lease(lease_id)
            .await
            .map_err(|_| "lease unavailable")?
            .ok_or("lease not found")?;
        if !lease_binding_matches(
            &lease,
            owner_device_id,
            host_installation_id,
            session_id,
            conversation_id,
            session_generation,
            source_id,
            geometry_revision,
        ) || lease.status != "active"
            || lease.expires_at.as_str() <= now
        {
            return Err("lease binding is stale");
        }
        let session = self
            .computer_session(session_id)
            .await
            .map_err(|_| "session unavailable")?
            .ok_or("session not found")?;
        if session.state != ComputerSessionState::Live.as_str()
            || session.generation != session_generation
            || session.geometry_revision != geometry_revision
            || session.source_id.as_deref() != source_id
        {
            return Err("computer session is stale");
        }
        Ok(lease)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_computer_input_batch(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        host_installation_id: &str,
        session_id: &str,
        conversation_id: &str,
        session_generation: u64,
        source_id: Option<&str>,
        geometry_revision: u64,
        sequence: u64,
        now: &str,
    ) -> Result<StoredComputerControlLease, &'static str> {
        let lease = self
            .validate_computer_input_lease(
                lease_id,
                owner_device_id,
                host_installation_id,
                session_id,
                conversation_id,
                session_generation,
                source_id,
                geometry_revision,
                now,
            )
            .await?;
        let next_sequence = lease.last_sequence.saturating_add(1);
        if sequence != next_sequence && sequence != lease.last_sequence {
            return Err("input sequence must increase by one");
        }
        Ok(lease)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn advance_computer_input_batch(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        host_installation_id: &str,
        session_id: &str,
        conversation_id: &str,
        session_generation: u64,
        source_id: Option<&str>,
        geometry_revision: u64,
        sequence: u64,
        now: &str,
    ) -> Result<StoredComputerControlLease, &'static str> {
        let lease = self
            .validate_computer_input_lease(
                lease_id,
                owner_device_id,
                host_installation_id,
                session_id,
                conversation_id,
                session_generation,
                source_id,
                geometry_revision,
                now,
            )
            .await?;
        if sequence == lease.last_sequence {
            return Ok(lease);
        }
        if sequence != lease.last_sequence.saturating_add(1) {
            return Err("input sequence must increase by one");
        }
        let result = sqlx::query("UPDATE computer_control_leases SET last_sequence=?, updated_at=? WHERE id=? AND status='active' AND last_sequence=? AND expires_at>? AND EXISTS(SELECT 1 FROM computer_sessions WHERE id=? AND state='live' AND generation=? AND geometry_revision=? AND source_id IS ?)")
            .bind(sequence as i64)
            .bind(now)
            .bind(lease_id)
            .bind(lease.last_sequence as i64)
            .bind(now)
            .bind(session_id)
            .bind(session_generation as i64)
            .bind(geometry_revision as i64)
            .bind(source_id)
            .execute(&self.pool)
            .await
            .map_err(|_| "lease unavailable")?;
        if result.rows_affected() != 1 {
            return Err("input sequence was not accepted");
        }
        self.computer_control_lease(lease_id)
            .await
            .map_err(|_| "lease unavailable")?
            .ok_or("lease ended")
    }

    /// Validate + advance in one step, for focused store tests. The daemon
    /// uses prepare + helper delivery + advance so durable state never moves
    /// ahead of native input acknowledgement.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub async fn accept_computer_input_batch(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        host_installation_id: &str,
        session_id: &str,
        conversation_id: &str,
        session_generation: u64,
        source_id: Option<&str>,
        geometry_revision: u64,
        sequence: u64,
        now: &str,
    ) -> Result<StoredComputerControlLease, &'static str> {
        let lease = self
            .validate_computer_input_lease(
                lease_id,
                owner_device_id,
                host_installation_id,
                session_id,
                conversation_id,
                session_generation,
                source_id,
                geometry_revision,
                now,
            )
            .await?;
        if sequence != lease.last_sequence.saturating_add(1) {
            return Err("input sequence must increase by one");
        }
        self.advance_computer_input_batch(
            lease_id,
            owner_device_id,
            host_installation_id,
            session_id,
            conversation_id,
            session_generation,
            source_id,
            geometry_revision,
            sequence,
            now,
        )
        .await
    }

    pub async fn release_computer_control_lease(
        &self,
        lease_id: &str,
        owner_device_id: &str,
        now: &str,
    ) -> Result<Option<StoredComputerControlLease>, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = Self::lease_by_id_with(&mut *transaction, lease_id).await? else {
            transaction.commit().await?;
            return Ok(None);
        };
        if current.owner_device_id != owner_device_id {
            transaction.commit().await?;
            return Ok(None);
        }
        if current.status == "active" {
            sqlx::query("UPDATE computer_control_leases SET status='released', updated_at=?, released_at=? WHERE id=? AND owner_device_id=? AND status='active'")
                .bind(now)
                .bind(now)
                .bind(lease_id)
                .bind(owner_device_id)
                .execute(&mut *transaction)
                .await?;
        }
        let released = Self::lease_by_id_with(&mut *transaction, lease_id).await?;
        transaction.commit().await?;
        Ok(released)
    }

    pub async fn release_computer_control_lease_for_session(
        &self,
        session_id: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE computer_control_leases SET status='released', updated_at=?, released_at=? WHERE session_id=? AND status='active'")
            .bind(now)
            .bind(now)
            .bind(session_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Conversations of included Projects and the owner's host-level view.
    pub async fn computer_conversation_allowed(
        &self,
        conversation_id: &str,
    ) -> Result<bool, sqlx::Error> {
        if conversation_id == HOST_VIEW_CONVERSATION_ID {
            return Ok(true);
        }
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_conversations pc JOIN projects p ON p.id=pc.project_id WHERE pc.conversation_id=? AND p.is_included=1)")
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn computer_session_by_request(
        &self,
        owner_device_id: &str,
        client_request_id: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let row = sqlx::query("SELECT id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, COALESCE(video_quality_v2, video_quality) AS video_quality, video_max_fps, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, ended_at FROM computer_sessions WHERE owner_device_id=? AND client_request_id=?")
        .bind(owner_device_id)
        .bind(client_request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(stored))
    }

    pub async fn computer_session(
        &self,
        id: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let row = sqlx::query("SELECT id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, COALESCE(video_quality_v2, video_quality) AS video_quality, video_max_fps, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, ended_at FROM computer_sessions WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(stored))
    }

    pub async fn active_computer_session_for_host(
        &self,
        host_installation_id: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let row = sqlx::query("SELECT id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, COALESCE(video_quality_v2, video_quality) AS video_quality, video_max_fps, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, ended_at FROM computer_sessions WHERE host_installation_id=? AND state IN ('preparing', 'awaitingSource', 'live', 'paused', 'stale') ORDER BY updated_at DESC LIMIT 1")
            .bind(host_installation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(stored))
    }

    pub async fn active_computer_sessions_for_host(
        &self,
        host_installation_id: &str,
    ) -> Result<Vec<StoredComputerSession>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, COALESCE(video_quality_v2, video_quality) AS video_quality, video_max_fps, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, ended_at FROM computer_sessions WHERE host_installation_id=? AND state IN ('preparing', 'awaitingSource', 'live', 'paused', 'stale') ORDER BY updated_at DESC")
            .bind(host_installation_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(stored).collect())
    }

    pub async fn insert_computer_session(
        &self,
        request: &ComputerSessionCreate,
    ) -> Result<StoredComputerSession, sqlx::Error> {
        sqlx::query("INSERT INTO computer_sessions (id, client_request_id, owner_device_id, host_installation_id, conversation_id, generation, state, video_quality, source_id, source_name, source_kind, source_width, source_height, source_scale, crop_json, geometry_revision, failure_reason, created_at, updated_at, last_state_at, video_quality_v2, video_max_fps) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&request.id)
            .bind(&request.client_request_id)
            .bind(&request.owner_device_id)
            .bind(&request.host_installation_id)
            .bind(&request.conversation_id)
            .bind(request.generation as i64)
            .bind(request.state.as_str())
            .bind(if request.video_quality == "medium" { "standard" } else { request.video_quality.as_str() })
            .bind(&request.source_id)
            .bind(&request.source_name)
            .bind(&request.source_kind)
            .bind(request.source_width.map(i64::from))
            .bind(request.source_height.map(i64::from))
            .bind(request.source_scale)
            .bind(&request.crop_json)
            .bind(request.geometry_revision as i64)
            .bind(&request.failure_reason)
            .bind(&request.now)
            .bind(&request.now)
            .bind(&request.now)
            .bind(&request.video_quality)
            .bind(i64::from(request.video_max_fps))
            .execute(&self.pool)
            .await?;
        self.computer_session(&request.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn update_computer_session_video_quality(
        &self,
        id: &str,
        owner_device_id: &str,
        generation: u64,
        quality: &str,
        max_fps: u16,
        now: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let result = sqlx::query("UPDATE computer_sessions SET video_quality=?, video_quality_v2=?, video_max_fps=?, updated_at=? WHERE id=? AND owner_device_id=? AND generation=? AND state IN ('live', 'paused')")
            .bind(if quality == "medium" { "standard" } else { quality })
            .bind(quality)
            .bind(i64::from(max_fps))
            .bind(now)
            .bind(id)
            .bind(owner_device_id)
            .bind(generation as i64)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.computer_session(id).await
    }

    /// Move the view and its active lease together. The helper has already
    /// drained held input and acknowledged this geometry; callers serialize
    /// native delivery through the commit. Never recreate a lease or reset its
    /// sequence, consent request or replay history when changing views.
    pub async fn rebind_computer_session_source(
        &self,
        expected: &StoredComputerSession,
        selected: &StoredComputerSession,
        lease_id: Option<&str>,
        now: &str,
        expires_at: &str,
    ) -> Result<Option<(StoredComputerSession, Option<StoredComputerControlLease>)>, sqlx::Error>
    {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) =
            Self::computer_session_by_id_with(&mut *transaction, &expected.id).await?
        else {
            return Ok(None);
        };
        if current.owner_device_id != expected.owner_device_id
            || current.host_installation_id != expected.host_installation_id
            || current.conversation_id != expected.conversation_id
            || current.generation != expected.generation
            || current.source_id != expected.source_id
            || current.geometry_revision != expected.geometry_revision
            || current.state != "live"
            || selected.geometry_revision <= current.geometry_revision
            || selected.source_id.is_none()
        {
            return Ok(None);
        }
        sqlx::query("UPDATE computer_control_leases SET status='expired', updated_at=?, released_at=? WHERE session_id=? AND status='active' AND expires_at<=?")
            .bind(now).bind(now).bind(&current.id).bind(now).execute(&mut *transaction).await?;
        let active = sqlx::query(
            "SELECT id FROM computer_control_leases WHERE session_id=? AND status='active'",
        )
        .bind(&current.id)
        .fetch_optional(&mut *transaction)
        .await?;
        if active
            .as_ref()
            .map(|row| row.get::<String, _>("id"))
            .as_deref()
            != lease_id
        {
            return Ok(None);
        }
        if let Some(id) = lease_id {
            let lease = Self::lease_by_id_with(&mut *transaction, id)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            if !lease_binding_matches(
                &lease,
                &current.owner_device_id,
                &current.host_installation_id,
                &current.id,
                &current.conversation_id,
                current.generation,
                current.source_id.as_deref(),
                current.geometry_revision,
            ) || lease.expires_at.as_str() <= now
            {
                return Ok(None);
            }
            sqlx::query("UPDATE computer_control_leases SET source_id=?, geometry_revision=?, updated_at=?, expires_at=? WHERE id=?")
                .bind(&selected.source_id).bind(selected.geometry_revision as i64).bind(now).bind(expires_at).bind(id)
                .execute(&mut *transaction).await?;
        }
        sqlx::query("UPDATE computer_sessions SET source_id=?, source_name=?, source_kind=?, source_width=?, source_height=?, source_scale=?, crop_json=?, geometry_revision=?, updated_at=? WHERE id=?")
            .bind(&selected.source_id).bind(&selected.source_name).bind(&selected.source_kind)
            .bind(selected.source_width.map(i64::from)).bind(selected.source_height.map(i64::from))
            .bind(selected.source_scale).bind(&selected.crop_json).bind(selected.geometry_revision as i64)
            .bind(now).bind(&current.id).execute(&mut *transaction).await?;
        let updated = Self::computer_session_by_id_with(&mut *transaction, &current.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        let lease = if let Some(id) = lease_id {
            Self::lease_by_id_with(&mut *transaction, id).await?
        } else {
            None
        };
        transaction.commit().await?;
        Ok(Some((updated, lease)))
    }

    /// Applies helper state only to the expected session generation. A stale
    /// helper event returns `None` and cannot resurrect an ended session. A
    /// failed session keeps its first failure reason through later failures.
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_computer_session(
        &self,
        id: &str,
        expected_generation: u64,
        state: ComputerSessionState,
        source_id: Option<&str>,
        source_name: Option<&str>,
        source_kind: Option<&str>,
        source_width: Option<u32>,
        source_height: Option<u32>,
        source_scale: Option<f64>,
        crop_json: Option<&str>,
        geometry_revision: u64,
        failure_reason: Option<&str>,
        now: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let result = sqlx::query("UPDATE computer_sessions SET state=?, source_id=COALESCE(?, source_id), source_name=COALESCE(?, source_name), source_kind=COALESCE(?, source_kind), source_width=COALESCE(?, source_width), source_height=COALESCE(?, source_height), source_scale=COALESCE(?, source_scale), crop_json=COALESCE(?, crop_json), geometry_revision=MAX(geometry_revision, ?), failure_reason=CASE WHEN state='failed' AND ?='failed' AND failure_reason IS NOT NULL THEN failure_reason ELSE ? END, updated_at=?, last_state_at=CASE WHEN state != ? THEN ? ELSE last_state_at END WHERE id=? AND generation=? AND state != 'ended' AND (geometry_revision<=? OR source_id IS NULL)")
            .bind(state.as_str())
            .bind(source_id)
            .bind(source_name)
            .bind(source_kind)
            .bind(source_width.map(i64::from))
            .bind(source_height.map(i64::from))
            .bind(source_scale)
            .bind(crop_json)
            .bind(geometry_revision as i64)
            .bind(state.as_str())
            .bind(failure_reason)
            .bind(now)
            .bind(state.as_str())
            .bind(now)
            .bind(id)
            .bind(expected_generation as i64)
            .bind(geometry_revision as i64)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.computer_session(id).await
    }

    pub async fn end_computer_session(
        &self,
        id: &str,
        owner_device_id: &str,
        expected_generation: Option<u64>,
        now: &str,
    ) -> Result<Option<StoredComputerSession>, sqlx::Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = Self::computer_session_by_id_with(&mut *transaction, id).await? else {
            transaction.commit().await?;
            return Ok(None);
        };
        if current.owner_device_id != owner_device_id {
            transaction.commit().await?;
            return Ok(None);
        }
        if current.state == ComputerSessionState::Ended.as_str() {
            transaction.commit().await?;
            return Ok(Some(current));
        }
        if expected_generation.is_some_and(|generation| generation != current.generation) {
            return Err(sqlx::Error::Protocol(
                "computer session generation mismatch".into(),
            ));
        }
        sqlx::query("UPDATE computer_sessions SET state='ended', generation=generation+1, updated_at=?, last_state_at=?, ended_at=? WHERE id=? AND owner_device_id=?")
            .bind(now).bind(now).bind(now).bind(id).bind(owner_device_id)
            .execute(&mut *transaction).await?;
        let ended = Self::computer_session_by_id_with(&mut *transaction, id).await?;
        transaction.commit().await?;
        Ok(ended)
    }
}

#[allow(clippy::too_many_arguments)]
fn lease_binding_matches(
    lease: &StoredComputerControlLease,
    owner_device_id: &str,
    host_installation_id: &str,
    session_id: &str,
    conversation_id: &str,
    session_generation: u64,
    source_id: Option<&str>,
    geometry_revision: u64,
) -> bool {
    lease.owner_device_id == owner_device_id
        && lease.host_installation_id == host_installation_id
        && lease.session_id == session_id
        && lease.conversation_id == conversation_id
        && lease.session_generation == session_generation
        && lease.source_id.as_deref() == source_id
        && lease.geometry_revision == geometry_revision
}

fn lease_matches_create(
    lease: &StoredComputerControlLease,
    request: &ComputerControlLeaseCreate,
) -> bool {
    lease.client_request_id == request.client_request_id
        && lease_binding_matches(
            lease,
            &request.owner_device_id,
            &request.host_installation_id,
            &request.session_id,
            &request.conversation_id,
            request.session_generation,
            request.source_id.as_deref(),
            request.geometry_revision,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_view_keeps_its_first_failure_reason() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let view = store
            .insert_computer_session(&ComputerSessionCreate {
                id: "view".into(),
                client_request_id: "view-request".into(),
                owner_device_id: "phone".into(),
                host_installation_id: "host".into(),
                conversation_id: HOST_VIEW_CONVERSATION_ID.into(),
                generation: 1,
                state: ComputerSessionState::Preparing,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: None,
                source_name: None,
                source_kind: None,
                source_width: None,
                source_height: None,
                source_scale: None,
                crop_json: None,
                geometry_revision: 0,
                failure_reason: None,
                now: "01".into(),
            })
            .await
            .unwrap();
        let fail = |reason: &'static str, now: &'static str| {
            let store = store.clone();
            async move {
                store
                    .transition_computer_session(
                        "view",
                        1,
                        ComputerSessionState::Failed,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        0,
                        Some(reason),
                        now,
                    )
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        assert_eq!(
            fail("permission_denied", "02")
                .await
                .failure_reason
                .as_deref(),
            Some("permission_denied")
        );
        // The helper exiting afterwards does not hide why the view failed.
        assert_eq!(
            fail("helper_exited", "03").await.failure_reason.as_deref(),
            Some("permission_denied")
        );
        assert_eq!(view.failure_reason, None);
    }

    #[tokio::test]
    async fn source_rebind_commits_session_and_lease_together_without_resetting_sequence() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let initial = store
            .insert_computer_session(&ComputerSessionCreate {
                id: "view".into(),
                client_request_id: "view-request".into(),
                owner_device_id: "phone".into(),
                host_installation_id: "host".into(),
                conversation_id: HOST_VIEW_CONVERSATION_ID.into(),
                generation: 1,
                state: ComputerSessionState::Live,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: Some("display:1".into()),
                source_name: Some("Screen".into()),
                source_kind: Some("display".into()),
                source_width: Some(1920),
                source_height: Some(1080),
                source_scale: Some(1.0),
                crop_json: None,
                geometry_revision: 1,
                failure_reason: None,
                now: "01".into(),
            })
            .await
            .unwrap();
        let ComputerLeaseAcquireResult::Granted(lease) = store
            .acquire_computer_control_lease(&ComputerControlLeaseCreate {
                id: "lease".into(),
                client_request_id: "consent".into(),
                session_id: initial.id.clone(),
                owner_device_id: initial.owner_device_id.clone(),
                host_installation_id: initial.host_installation_id.clone(),
                conversation_id: initial.conversation_id.clone(),
                session_generation: 1,
                source_id: initial.source_id.clone(),
                geometry_revision: 1,
                now: "01".into(),
                expires_at: "09".into(),
            })
            .await
            .unwrap()
        else {
            panic!("lease refused")
        };
        store
            .accept_computer_input_batch(
                &lease.id,
                "phone",
                "host",
                &initial.id,
                HOST_VIEW_CONVERSATION_ID,
                1,
                initial.source_id.as_deref(),
                1,
                1,
                "02",
            )
            .await
            .unwrap();
        let mut selected = initial.clone();
        selected.source_id = Some("app:editor#42".into());
        selected.source_kind = Some("window".into());
        selected.source_width = Some(800);
        selected.source_height = Some(600);
        selected.geometry_revision = 2;
        for wrong_lease in [None, Some("other")] {
            assert!(store
                .rebind_computer_session_source(&initial, &selected, wrong_lease, "03", "10")
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                store.computer_session(&initial.id).await.unwrap().unwrap(),
                initial
            );
            assert_eq!(
                store
                    .computer_control_lease(&lease.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .source_id,
                initial.source_id
            );
        }
        let (changed, rebound) = store
            .rebind_computer_session_source(&initial, &selected, Some(&lease.id), "03", "10")
            .await
            .unwrap()
            .unwrap();
        let rebound = rebound.unwrap();
        assert_eq!(changed.id, initial.id);
        assert_eq!(changed.generation, 1);
        assert_eq!(rebound.id, lease.id);
        assert_eq!(rebound.client_request_id, lease.client_request_id);
        assert_eq!(rebound.acquired_at, lease.acquired_at);
        assert_eq!(rebound.last_sequence, 1);
        assert_eq!(rebound.source_id, changed.source_id);
        assert_eq!(rebound.geometry_revision, changed.geometry_revision);
        assert!(
            store
                .transition_computer_session(
                    &initial.id,
                    1,
                    ComputerSessionState::Live,
                    initial.source_id.as_deref(),
                    None,
                    Some("display"),
                    None,
                    None,
                    None,
                    None,
                    1,
                    None,
                    "04"
                )
                .await
                .unwrap()
                .is_none(),
            "A delayed helper event cannot restore the previous source"
        );
        assert_eq!(
            store.computer_session(&initial.id).await.unwrap().unwrap(),
            changed
        );

        assert!(
            store
                .rebind_computer_session_source(&initial, &selected, Some(&lease.id), "04", "11")
                .await
                .unwrap()
                .is_none(),
            "Old view cannot rebind again"
        );
        assert!(store
            .prepare_computer_input_batch(
                &lease.id,
                "phone",
                "host",
                &initial.id,
                HOST_VIEW_CONVERSATION_ID,
                1,
                initial.source_id.as_deref(),
                1,
                2,
                "04"
            )
            .await
            .is_err());
        assert!(store
            .heartbeat_computer_control_lease(
                &lease.id,
                "phone",
                "host",
                &initial.id,
                HOST_VIEW_CONVERSATION_ID,
                1,
                initial.source_id.as_deref(),
                1,
                "04",
                "11"
            )
            .await
            .unwrap()
            .is_none());
        // A duplicate receipt retains its sequence; the next batch continues.
        assert_eq!(
            store
                .advance_computer_input_batch(
                    &lease.id,
                    "phone",
                    "host",
                    &initial.id,
                    HOST_VIEW_CONVERSATION_ID,
                    1,
                    changed.source_id.as_deref(),
                    2,
                    1,
                    "04"
                )
                .await
                .unwrap()
                .last_sequence,
            1
        );
        assert_eq!(
            store
                .advance_computer_input_batch(
                    &lease.id,
                    "phone",
                    "host",
                    &initial.id,
                    HOST_VIEW_CONVERSATION_ID,
                    1,
                    changed.source_id.as_deref(),
                    2,
                    2,
                    "04"
                )
                .await
                .unwrap()
                .last_sequence,
            2
        );
        store
            .release_computer_control_lease(&lease.id, "phone", "05")
            .await
            .unwrap();
        let mut screen = changed.clone();
        screen.source_id = Some("display:1".into());
        screen.source_kind = Some("display".into());
        screen.geometry_revision = 3;
        let (viewing, control) = store
            .rebind_computer_session_source(&changed, &screen, None, "05", "12")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(viewing.id, initial.id);
        assert_eq!(viewing.source_id, screen.source_id);
        assert!(
            control.is_none(),
            "Viewing a different source must never acquire control"
        );
    }

    #[tokio::test]
    async fn computer_session_is_idempotent_and_end_retires_generation() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let request = ComputerSessionCreate {
            id: "session-1".into(),
            client_request_id: "request-1".into(),
            owner_device_id: "phone-1".into(),
            host_installation_id: "host-1".into(),
            conversation_id: "project-chat".into(),
            generation: 1,
            state: ComputerSessionState::Unavailable,
            video_quality: "medium".into(),
            video_max_fps: 30,
            source_id: Some("display-1".into()),
            source_name: Some("Mac display".into()),
            source_kind: Some("display".into()),
            source_width: Some(1280),
            source_height: Some(720),
            source_scale: Some(2.0),
            crop_json: None,
            geometry_revision: 0,
            failure_reason: Some("provider unavailable".into()),
            now: "2026-09-12T00:00:00Z".into(),
        };
        let inserted = store.insert_computer_session(&request).await.unwrap();
        assert_eq!(inserted.state, "unavailable");
        assert_eq!(inserted.video_quality, "medium");
        assert_eq!(inserted.video_max_fps, 30);
        let same = store
            .computer_session_by_request("phone-1", "request-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(same.id, "session-1");
        assert_eq!(same.video_quality, "medium");
        let ended = store
            .end_computer_session("session-1", "phone-1", Some(1), "later")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended.state, "ended");
        assert_eq!(ended.generation, 2);
        assert!(store
            .end_computer_session("session-1", "phone-1", Some(1), "later")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn quality_change_requires_current_owner_generation_and_live_session() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let request = ComputerSessionCreate {
            id: "session-1".into(),
            client_request_id: "request-1".into(),
            owner_device_id: "phone-1".into(),
            host_installation_id: "host-1".into(),
            conversation_id: "project-chat".into(),
            generation: 1,
            state: ComputerSessionState::Live,
            video_quality: "standard".into(),
            video_max_fps: 15,
            source_id: None,
            source_name: None,
            source_kind: None,
            source_width: None,
            source_height: None,
            source_scale: None,
            crop_json: None,
            geometry_revision: 1,
            failure_reason: None,
            now: "now".into(),
        };
        store.insert_computer_session(&request).await.unwrap();
        assert!(store
            .update_computer_session_video_quality("session-1", "other", 1, "high", 15, "later")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .update_computer_session_video_quality("session-1", "phone-1", 2, "high", 15, "later")
            .await
            .unwrap()
            .is_none());
        let updated = store
            .update_computer_session_video_quality("session-1", "phone-1", 1, "medium", 60, "later")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.video_quality, "medium");
        assert_eq!(updated.video_max_fps, 60);
        assert_eq!(updated.geometry_revision, 1);
        store
            .end_computer_session("session-1", "phone-1", Some(1), "ended")
            .await
            .unwrap();
        assert!(store
            .update_computer_session_video_quality("session-1", "phone-1", 2, "high", 15, "later")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn helper_transition_is_compare_and_set_and_monotonic_for_geometry() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .insert_computer_session(&ComputerSessionCreate {
                id: "session-cas".into(),
                client_request_id: "request-cas".into(),
                owner_device_id: "phone-1".into(),
                host_installation_id: "host-1".into(),
                conversation_id: "project-chat".into(),
                generation: 1,
                state: ComputerSessionState::Preparing,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: None,
                source_name: None,
                source_kind: None,
                source_width: None,
                source_height: None,
                source_scale: None,
                crop_json: None,
                geometry_revision: 0,
                failure_reason: None,
                now: "now".into(),
            })
            .await
            .unwrap();

        let awaiting = store
            .transition_computer_session(
                "session-cas",
                1,
                ComputerSessionState::AwaitingSource,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                2,
                None,
                "later",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(awaiting.state, "awaitingSource");
        assert_eq!(awaiting.geometry_revision, 2);

        let retained = store
            .transition_computer_session(
                "session-cas",
                1,
                ComputerSessionState::Preparing,
                Some("display:1"),
                Some("Display 1"),
                Some("display"),
                Some(1280),
                Some(720),
                Some(2.0),
                Some(r#"{"x":0,"y":0,"width":1280,"height":720}"#),
                1,
                None,
                "older",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.state, "preparing");
        assert_eq!(retained.geometry_revision, 2);
        assert_eq!(retained.source_id.as_deref(), Some("display:1"));
        assert_eq!(retained.source_name.as_deref(), Some("Display 1"));
        assert_eq!(retained.source_kind.as_deref(), Some("display"));
        assert_eq!(
            retained.crop_json.as_deref(),
            Some(r#"{"x":0,"y":0,"width":1280,"height":720}"#)
        );

        assert!(store
            .transition_computer_session(
                "session-cas",
                2,
                ComputerSessionState::Failed,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                3,
                Some("stale_generation"),
                "stale",
            )
            .await
            .unwrap()
            .is_none());

        let ended = store
            .end_computer_session("session-cas", "phone-1", Some(1), "ended")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended.state, "ended");
        assert!(store
            .transition_computer_session(
                "session-cas",
                2,
                ComputerSessionState::Failed,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                4,
                Some("late_event"),
                "late",
            )
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn control_lease_is_host_wide_sequenced_and_release_is_idempotent() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .insert_computer_session(&ComputerSessionCreate {
                id: "session-live".into(),
                client_request_id: "session-request".into(),
                owner_device_id: "phone-1".into(),
                host_installation_id: "host-1".into(),
                conversation_id: "project-chat".into(),
                generation: 1,
                state: ComputerSessionState::Live,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: Some("display-1".into()),
                source_name: Some("Mac display".into()),
                source_kind: Some("display".into()),
                source_width: Some(1280),
                source_height: Some(720),
                source_scale: Some(2.0),
                crop_json: None,
                geometry_revision: 4,
                failure_reason: None,
                now: "2026-09-12T00:00:00Z".into(),
            })
            .await
            .unwrap();
        store
            .insert_computer_session(&ComputerSessionCreate {
                id: "session-live-2".into(),
                client_request_id: "session-request-2".into(),
                owner_device_id: "phone-2".into(),
                host_installation_id: "host-1".into(),
                conversation_id: "other-chat".into(),
                generation: 1,
                state: ComputerSessionState::Live,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: Some("display-1".into()),
                source_name: Some("Mac display".into()),
                source_kind: Some("display".into()),
                source_width: Some(1280),
                source_height: Some(720),
                source_scale: Some(2.0),
                crop_json: None,
                geometry_revision: 4,
                failure_reason: None,
                now: "2026-09-12T00:00:00Z".into(),
            })
            .await
            .unwrap();
        let request = ComputerControlLeaseCreate {
            id: "lease-1".into(),
            client_request_id: "control-request-1".into(),
            session_id: "session-live".into(),
            owner_device_id: "phone-1".into(),
            host_installation_id: "host-1".into(),
            conversation_id: "project-chat".into(),
            session_generation: 1,
            source_id: Some("display-1".into()),
            geometry_revision: 4,
            now: "2026-09-12T00:00:01Z".into(),
            expires_at: "2026-09-12T00:00:11Z".into(),
        };
        let granted = store
            .acquire_computer_control_lease(&request)
            .await
            .unwrap();
        let lease = match granted {
            ComputerLeaseAcquireResult::Granted(lease) => lease,
            other => panic!("unexpected lease result: {other:?}"),
        };
        assert_eq!(lease.status, "active");
        assert_eq!(lease.last_sequence, 0);
        assert!(matches!(
            store
                .acquire_computer_control_lease(&request)
                .await
                .unwrap(),
            ComputerLeaseAcquireResult::Existing(_)
        ));
        assert_eq!(
            store
                .acquire_computer_control_lease(&ComputerControlLeaseCreate {
                    geometry_revision: 5,
                    ..request.clone()
                })
                .await
                .unwrap(),
            ComputerLeaseAcquireResult::Conflict
        );
        assert!(matches!(
            store
                .acquire_computer_control_lease(&ComputerControlLeaseCreate {
                    id: "lease-2".into(),
                    client_request_id: "control-request-2".into(),
                    owner_device_id: "phone-2".into(),
                    session_id: "session-live-2".into(),
                    conversation_id: "other-chat".into(),
                    ..request.clone()
                })
                .await
                .unwrap(),
            ComputerLeaseAcquireResult::Busy(_)
        ));
        assert_eq!(
            store
                .accept_computer_input_batch(
                    &lease.id,
                    "phone-1",
                    "host-1",
                    "session-live",
                    "project-chat",
                    1,
                    Some("display-1"),
                    4,
                    1,
                    "2026-09-12T00:00:02Z"
                )
                .await
                .unwrap()
                .last_sequence,
            1
        );
        assert_eq!(
            store
                .accept_computer_input_batch(
                    &lease.id,
                    "phone-1",
                    "host-1",
                    "session-live",
                    "project-chat",
                    1,
                    Some("display-1"),
                    4,
                    1,
                    "2026-09-12T00:00:03Z"
                )
                .await,
            Err("input sequence must increase by one")
        );
        // A helper can have delivered sequence 1 before the durable CAS
        // response was lost. Retrying the same request must be allowed to
        // finalize the already-advanced durable sequence without reinjection.
        assert_eq!(
            store
                .prepare_computer_input_batch(
                    &lease.id,
                    "phone-1",
                    "host-1",
                    "session-live",
                    "project-chat",
                    1,
                    Some("display-1"),
                    4,
                    1,
                    "2026-09-12T00:00:03Z"
                )
                .await
                .unwrap()
                .last_sequence,
            1
        );
        assert_eq!(
            store
                .advance_computer_input_batch(
                    &lease.id,
                    "phone-1",
                    "host-1",
                    "session-live",
                    "project-chat",
                    1,
                    Some("display-1"),
                    4,
                    1,
                    "2026-09-12T00:00:03Z"
                )
                .await
                .unwrap()
                .last_sequence,
            1
        );
        assert_eq!(
            store
                .accept_computer_input_batch(
                    &lease.id,
                    "phone-1",
                    "host-1",
                    "session-live",
                    "project-chat",
                    1,
                    Some("display-1"),
                    5,
                    2,
                    "2026-09-12T00:00:03Z"
                )
                .await,
            Err("lease binding is stale")
        );
        let released = store
            .release_computer_control_lease(&lease.id, "phone-1", "2026-09-12T00:00:04Z")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(released.status, "released");
        assert_eq!(
            store
                .release_computer_control_lease(&lease.id, "phone-1", "2026-09-12T00:00:05Z")
                .await
                .unwrap()
                .unwrap()
                .status,
            "released"
        );
    }
}
