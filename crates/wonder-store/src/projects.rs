//! Owner-selected source-folder projects and their native provider sessions.
//! Paths are validated by the daemon; this module keeps identity invariants.
use super::continuations::drop_usage_resume;
use super::*;

pub const EXECUTION_SCOPE_PROJECTS: &str = "projects";
pub const MAX_PROJECT_ROOTS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRootInput {
    pub path: String,
    pub canonical_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProjectRoot {
    pub id: String,
    pub path: String,
    pub canonical_path: String,
    pub ordinal: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProject {
    pub id: String,
    pub name: String,
    pub is_included: bool,
    pub pin_order: Option<i64>,
    pub primary_root_id: String,
    pub roots_revision: i64,
    pub last_family: Option<AgentFamily>,
    pub created_at: String,
    pub updated_at: String,
    pub last_used_at: Option<String>,
    pub roots: Vec<StoredProjectRoot>,
    /// This Project's own auto-archive rule; `None` follows the host-wide one.
    pub auto_archive: Option<AutoArchiveRule>,
    /// The model last chosen in this Project, one per provider. A new chat
    /// with no choice of its own starts from it, ahead of the default.
    pub last_models: Vec<ProjectLastModel>,
    /// Fingerprints of this Project's own Claude Code configuration that the
    /// owner trusted, newest first. Claude loads a folder's hooks, settings
    /// and MCP servers only when its current fingerprint is listed.
    pub claude_trust: Vec<String>,
}

/// How many trusted Claude configurations one Project keeps.
const CLAUDE_TRUST_LIMIT: i64 = 16;

/// The model and effort last chosen for one provider in a Project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectLastModel {
    pub family: AgentFamily,
    pub model: String,
    pub effort: Option<String>,
}

pub struct NativeConversationOwner {
    pub family: AgentFamily,
    pub provider_store: String,
    pub native_session_id: String,
    pub conversation: StoredProjectConversation,
    pub is_current: bool,
}

impl Store {
    /// Host-wide discovery preference; older databases and new hosts default on.
    pub async fn include_worktrees(&self) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT include_worktrees FROM project_settings WHERE id=1",
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(true))
    }

    pub async fn set_include_worktrees(&self, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO project_settings(id,include_worktrees) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET include_worktrees=excluded.include_worktrees")
            .bind(enabled).execute(&self.pool).await?;
        Ok(())
    }

    /// Replaces the verified linked worktrees of one saved folder (its
    /// canonical path). Only wonderd's file-based verification writes here.
    pub async fn replace_project_worktrees(
        &self,
        root_path: &str,
        paths: &[String],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM project_worktrees WHERE root_path=?")
            .bind(root_path)
            .execute(&mut *tx)
            .await?;
        for path in paths {
            sqlx::query("INSERT OR IGNORE INTO project_worktrees(root_path,path) VALUES(?,?)")
                .bind(root_path)
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// The host-wide auto-archive rule; a database without one gets the defaults.
    pub async fn auto_archive_rule(&self) -> Result<AutoArchiveRule, sqlx::Error> {
        let row: Option<(i64, bool, Option<String>)> = sqlx::query_as(
            "SELECT auto_archive_days, auto_archive_on_merge, auto_archive_since FROM project_settings WHERE id=1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|(idle_days, on_merge, since)| AutoArchiveRule {
                idle_days,
                on_merge,
                since,
            })
            .unwrap_or(AutoArchiveRule {
                idle_days: 3,
                on_merge: true,
                since: None,
            }))
    }

    /// Saves the host-wide rule. `since` moves to `now` only when the rule
    /// turns on from off, so idle time never counts from before it.
    pub async fn set_auto_archive_rule(
        &self,
        idle_days: i64,
        on_merge: bool,
        now: &str,
    ) -> Result<AutoArchiveRule, sqlx::Error> {
        if !(0..=90).contains(&idle_days) {
            return Err(invalid("Choose between 0 and 90 days"));
        }
        sqlx::query("INSERT INTO project_settings(id,include_worktrees,auto_archive_days,auto_archive_on_merge,auto_archive_since) VALUES(1,1,?,?,?) ON CONFLICT(id) DO UPDATE SET auto_archive_since=CASE WHEN (auto_archive_days=0 AND excluded.auto_archive_days>0) OR (auto_archive_on_merge=0 AND excluded.auto_archive_on_merge=1) OR auto_archive_since IS NULL THEN excluded.auto_archive_since ELSE auto_archive_since END, auto_archive_days=excluded.auto_archive_days, auto_archive_on_merge=excluded.auto_archive_on_merge")
            .bind(idle_days).bind(on_merge).bind(now).execute(&self.pool).await?;
        self.auto_archive_rule().await
    }

    /// Sets a Project's own rule, or with `None` returns it to the host-wide
    /// rule. Like the host rule, `since` moves only when the rule turns on.
    pub async fn set_project_auto_archive_rule(
        &self,
        project: &str,
        rule: Option<(i64, bool)>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        write_project_auto_archive_rule(&mut conn, project, rule, now).await
    }

    /// Remembers the model and effort the owner chose for `family` in a
    /// Project: starting a thread with it, or choosing it in one.
    pub async fn record_project_model(
        &self,
        project: &str,
        family: AgentFamily,
        model: &str,
        effort: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let model = model.trim();
        if model.is_empty() {
            return Ok(());
        }
        sqlx::query("INSERT INTO project_last_models(project_id,agent_family,model,effort) VALUES(?,?,?,?) ON CONFLICT(project_id,agent_family) DO UPDATE SET model=excluded.model, effort=excluded.effort")
            .bind(project).bind(family.as_str()).bind(model).bind(effort.map(str::trim).filter(|e| !e.is_empty()))
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Trusts one fingerprint of a Project folder's own Claude configuration.
    /// A Project keeps its newest `CLAUDE_TRUST_LIMIT` (one per folder or
    /// worktree it runs in); older ones need trusting again.
    pub async fn trust_project_claude_configuration(
        &self,
        project: &str,
        fingerprint: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO project_claude_trust(project_id,fingerprint,trusted_at) VALUES(?,?,?) ON CONFLICT(project_id,fingerprint) DO UPDATE SET trusted_at=excluded.trusted_at")
            .bind(project).bind(fingerprint).bind(now).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM project_claude_trust WHERE project_id=? AND fingerprint NOT IN (SELECT fingerprint FROM project_claude_trust WHERE project_id=? ORDER BY trusted_at DESC, fingerprint LIMIT ?)")
            .bind(project).bind(project).bind(CLAUDE_TRUST_LIMIT).execute(&mut *tx).await?;
        tx.commit().await
    }

    /// Withdraws trust from every configuration of a Project's folders.
    pub async fn revoke_project_claude_configuration(
        &self,
        project: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM project_claude_trust WHERE project_id=?")
            .bind(project)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// When the owner last sent a message in this conversation from Cupola.
    pub async fn last_sent_message_at(
        &self,
        conversation: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT MAX(created_at) FROM messages WHERE conversation_id=?")
            .bind(conversation)
            .fetch_one(&self.pool)
            .await
    }
}

/// Writes a Project's own rule (`None` follows the host-wide rule). Like the
/// host rule, `since` moves only when the rule turns on.
async fn write_project_auto_archive_rule(
    conn: &mut sqlx::SqliteConnection,
    project: &str,
    rule: Option<(i64, bool)>,
    now: &str,
) -> Result<(), sqlx::Error> {
    let Some((idle_days, on_merge)) = rule else {
        sqlx::query("DELETE FROM project_auto_archive WHERE project_id=?")
            .bind(project)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    };
    if !(0..=90).contains(&idle_days) {
        return Err(invalid("Choose between 0 and 90 days"));
    }
    // A Project that followed the host rule keeps counting from the host's
    // start unless that rule was off.
    sqlx::query("INSERT INTO project_auto_archive(project_id,idle_days,on_merge,since) VALUES(?,?,?,?) ON CONFLICT(project_id) DO UPDATE SET since=CASE WHEN (idle_days=0 AND excluded.idle_days>0) OR (on_merge=0 AND excluded.on_merge=1) THEN excluded.since ELSE since END, idle_days=excluded.idle_days, on_merge=excluded.on_merge")
        .bind(project).bind(idle_days).bind(on_merge).bind(now).execute(&mut *conn).await?;
    Ok(())
}

/// When Project threads archive themselves: after `idle_days` without
/// activity (0 = never) and when their pull request merges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoArchiveRule {
    pub idle_days: i64,
    pub on_merge: bool,
    /// When the rule last turned on; idle time never counts from before it.
    pub since: Option<String>,
}

impl StoredProject {
    pub fn primary_root(&self) -> &StoredProjectRoot {
        self.roots
            .iter()
            .find(|root| root.id == self.primary_root_id)
            .unwrap_or(&self.roots[0])
    }
    /// Exact membership in the saved folders, never string-prefix matching.
    /// A thread's cwd may be a discovered worktree; wonderd resolves it with
    /// `projects::thread_project` before calling this.
    pub fn root_for(&self, path: &str) -> Option<&StoredProjectRoot> {
        self.roots
            .iter()
            .find(|root| root.path == path || root.canonical_path == path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectCreate {
    Created(StoredProject),
    Existing(StoredProject),
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProjectConversation {
    pub conversation_id: String,
    pub project_id: String,
    pub family: AgentFamily,
    pub provider_store: String,
    pub native_session_id: Option<String>,
    pub cwd: String,
    pub roots_revision: i64,
    pub title: String,
    /// The owner renamed the thread, or Cupola gave it its title (a fork's,
    /// a child task's name): shown as it is. Otherwise the title was taken
    /// from the first message or a provider's name and follows the list rule.
    pub title_chosen: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub access_mode: String,
    /// How a Claude thread asks before acting; always `ask` for Codex threads.
    pub claude_approval: String,
    pub plan_mode: bool,
    /// The newest native turn made outside Wonder whose settings this thread
    /// already took, so a later change on the phone wins until the desktop
    /// app runs another turn.
    pub native_settings_turn: Option<String>,
    pub is_pinned: bool,
    pub has_unread: bool,
    /// Archived in Wonder only (Claude Code threads); see migration 0096.
    pub wonder_archived: bool,
    pub creation_request_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_activity_at: String,
}

#[derive(Clone, Debug)]
pub struct ProjectConversationInsert<'a> {
    pub conversation_id: &'a str,
    pub project_id: &'a str,
    pub family: AgentFamily,
    pub provider_store: &'a str,
    pub native_session_id: Option<&'a str>,
    pub cwd: &'a str,
    pub roots_revision: i64,
    pub title: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub service_tier: Option<&'a str>,
    pub access_mode: &'a str,
    pub claude_approval: &'a str,
    pub plan_mode: bool,
    pub creation_request_id: Option<&'a str>,
    pub now: &'a str,
}

/// Fields a PATCH may change; `None` leaves a field as it is.
#[derive(Clone, Debug, Default)]
pub struct ProjectConversationPatch<'a> {
    pub title: Option<&'a str>,
    pub pinned: Option<bool>,
    pub unread: Option<bool>,
    pub model: Option<&'a str>,
    /// Omitted preserves the current effort; `Some(None)` clears it.
    pub effort: Option<Option<&'a str>>,
    /// Omitted preserves speed; `Some(None)` returns to Standard.
    pub service_tier: Option<Option<&'a str>>,
    pub access_mode: Option<&'a str>,
    pub claude_approval: Option<&'a str>,
    pub plan_mode: Option<bool>,
    pub native_settings_turn: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectConversationCreate {
    Created(StoredProjectConversation),
    Existing(StoredProjectConversation),
}

fn project_creation_payload(
    insert: &ProjectConversationInsert<'_>,
    request_digest: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!([
        insert.project_id,
        insert.family.as_str(),
        insert.provider_store,
        insert.cwd,
        insert.roots_revision,
        insert.model,
        insert.effort,
        insert.access_mode,
        insert.claude_approval,
        i64::from(insert.plan_mode)
    ]);
    // Migration 0080 and existing requests use ten fields when speed is omitted.
    if let Some(tier) = insert.service_tier {
        payload
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(tier));
    }
    match request_digest {
        Some(digest) => serde_json::json!({
            "version": 2,
            "resolved": payload,
            "requestDigest": digest,
        }),
        None => payload,
    }
}

fn matching_project_creation(
    row: &sqlx::sqlite::SqliteRow,
    payload: &serde_json::Value,
) -> Result<StoredProjectConversation, sqlx::Error> {
    let original: Option<String> = row.get("creation_payload");
    let original =
        original.and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
    if original.as_ref() != Some(payload) {
        return Err(invalid("creation_request_conflict"));
    }
    project_conversation(row)
}

fn invalid(message: &str) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}

pub(crate) fn project_conversation(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<StoredProjectConversation, sqlx::Error> {
    Ok(StoredProjectConversation {
        conversation_id: row.get("conversation_id"),
        project_id: row.get("project_id"),
        family: family(row.get("agent_family"))?,
        provider_store: row.get("provider_store"),
        native_session_id: row.get("native_session_id"),
        cwd: row.get("cwd"),
        roots_revision: row.get("roots_revision"),
        title: row.get("title"),
        // Absent before migration 0121, which upgrade tests read through.
        title_chosen: row.try_get::<i64, _>("title_chosen").unwrap_or(0) != 0,
        model: row.get("model"),
        effort: row.get("effort"),
        service_tier: row.get("service_tier"),
        access_mode: row.get("access_mode"),
        claude_approval: row.get("claude_approval"),
        plan_mode: row.get::<i64, _>("plan_mode") != 0,
        // Absent before migration 0091, which upgrade tests read through.
        native_settings_turn: row.try_get("native_settings_turn").unwrap_or(None),
        is_pinned: row.get::<i64, _>("is_pinned") != 0,
        has_unread: row.get::<i64, _>("has_unread") != 0,
        // Absent before migration 0096, which upgrade tests read through.
        wonder_archived: row
            .try_get::<Option<String>, _>("wonder_archived_at")
            .ok()
            .flatten()
            .is_some(),
        creation_request_id: row.get("creation_request_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_activity_at: row.get("last_activity_at"),
    })
}

fn family(value: &str) -> Result<AgentFamily, sqlx::Error> {
    AgentFamily::from_storage(value)
}

/// A full path in the host's own style: `/…`, or a Windows drive path such as
/// `C:\Work` or `C:/Work` (the host checks the style of the computer it runs on).
fn is_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

fn validate_roots(roots: &[ProjectRootInput], primary: usize) -> Result<(), sqlx::Error> {
    if roots.is_empty() || roots.len() > MAX_PROJECT_ROOTS || primary >= roots.len() {
        return Err(invalid("A project needs between one and sixteen folders"));
    }
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        if !is_absolute_path(&root.path)
            || !is_absolute_path(&root.canonical_path)
            || root.path.len() > 4096
            || root.canonical_path.len() > 4096
            || !seen.insert(root.canonical_path.as_str())
        {
            return Err(invalid("Project folders must be distinct absolute paths"));
        }
    }
    Ok(())
}

const ACCESS_MODES: [&str; 3] = ["read_only", "workspace", "full_access"];

impl Store {
    /// A current included Project owns a thread directly or through an exact
    /// chain of native thread, family, and provider-store bindings.
    pub async fn owning_project_conversation(
        &self,
        conversation: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        sqlx::query(concat!(
            "SELECT * FROM project_conversations WHERE conversation_id=(",
            crate::questions::verified_subagent_root_sql!(),
            ")"
        ))
        .bind(conversation)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(project_conversation)
        .transpose()
    }

    async fn load_project(
        &self,
        executor: &mut sqlx::SqliteConnection,
        id: &str,
    ) -> Result<Option<StoredProject>, sqlx::Error> {
        let Some(row) = sqlx::query("SELECT * FROM projects WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *executor)
            .await?
        else {
            return Ok(None);
        };
        let roots = sqlx::query("SELECT id,path,canonical_path,ordinal FROM project_roots WHERE project_id=? ORDER BY ordinal")
            .bind(id)
            .fetch_all(&mut *executor)
            .await?
            .iter()
            .map(|root| StoredProjectRoot {
                id: root.get("id"),
                path: root.get("path"),
                canonical_path: root.get("canonical_path"),
                ordinal: root.get("ordinal"),
            })
            .collect::<Vec<_>>();
        if roots.is_empty() {
            return Err(invalid("Project has no folders"));
        }
        Ok(Some(StoredProject {
            id: row.get("id"),
            name: row.get("name"),
            is_included: row.get::<i64, _>("is_included") != 0,
            pin_order: row.get("pin_order"),
            primary_root_id: row.get("primary_root_id"),
            roots_revision: row.get("roots_revision"),
            last_family: row
                .get::<Option<String>, _>("last_family")
                .as_deref()
                .map(family)
                .transpose()?,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            last_used_at: row.get("last_used_at"),
            roots,
            // Read by `project`, `list_projects` and metadata updates; a
            // Project being created or re-rooted has the rule it had.
            auto_archive: None,
            last_models: Vec::new(),
            claude_trust: Vec::new(),
        }))
    }

    async fn with_auto_archive(
        &self,
        executor: &mut sqlx::SqliteConnection,
        project: Option<StoredProject>,
    ) -> Result<Option<StoredProject>, sqlx::Error> {
        let Some(mut project) = project else {
            return Ok(None);
        };
        let rule: Option<(i64, bool, String)> = sqlx::query_as(
            "SELECT idle_days, on_merge, since FROM project_auto_archive WHERE project_id=?",
        )
        .bind(&project.id)
        .fetch_optional(&mut *executor)
        .await?;
        project.auto_archive = rule.map(|(idle_days, on_merge, since)| AutoArchiveRule {
            idle_days,
            on_merge,
            since: Some(since),
        });
        let models: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT agent_family, model, effort FROM project_last_models WHERE project_id=? ORDER BY agent_family",
        )
        .bind(&project.id)
        .fetch_all(&mut *executor)
        .await?;
        project.last_models = models
            .into_iter()
            .map(|(family_value, model, effort)| {
                Ok(ProjectLastModel {
                    family: family(&family_value)?,
                    model,
                    effort,
                })
            })
            .collect::<Result<_, sqlx::Error>>()?;
        project.claude_trust = sqlx::query_scalar(
            "SELECT fingerprint FROM project_claude_trust WHERE project_id=? ORDER BY trusted_at DESC, fingerprint",
        )
        .bind(&project.id)
        .fetch_all(&mut *executor)
        .await?;
        Ok(Some(project))
    }

    async fn insert_roots(
        tx: &mut sqlx::SqliteConnection,
        project: &str,
        roots: &[ProjectRootInput],
    ) -> Result<Vec<String>, sqlx::Error> {
        let mut ids = Vec::with_capacity(roots.len());
        for (ordinal, root) in roots.iter().enumerate() {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO project_roots(id,project_id,path,canonical_path,ordinal) VALUES(?,?,?,?,?)")
                .bind(&id).bind(project).bind(&root.path).bind(&root.canonical_path).bind(ordinal as i64)
                .execute(&mut *tx).await?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Metadata-only and idempotent by request ID; a changed payload conflicts.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_project(
        &self,
        id: &str,
        request_id: &str,
        payload_sha256: &str,
        name: &str,
        roots: &[ProjectRootInput],
        primary: usize,
        now: &str,
    ) -> Result<ProjectCreate, sqlx::Error> {
        validate_roots(roots, primary)?;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(invalid(
                "Project names must be between 1 and 120 characters",
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = sqlx::query(
            "SELECT id,creation_payload_sha256 FROM projects WHERE creation_request_id=?",
        )
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row
                .get::<Option<String>, _>("creation_payload_sha256")
                .as_deref()
                != Some(payload_sha256)
            {
                return Ok(ProjectCreate::Conflict);
            }
            let existing: String = row.get("id");
            let project = self
                .load_project(&mut tx, &existing)
                .await?
                .ok_or_else(|| invalid("missing project"))?;
            tx.commit().await?;
            return Ok(ProjectCreate::Existing(project));
        }
        // The primary root ID is known only after insert; the row is completed
        // before commit so no reader can observe a project without a primary.
        sqlx::query("INSERT INTO projects(id,name,primary_root_id,creation_request_id,creation_payload_sha256,created_at,updated_at) VALUES(?,?,'',?,?,?,?)")
            .bind(id).bind(name).bind(request_id).bind(payload_sha256).bind(now).bind(now)
            .execute(&mut *tx).await?;
        let ids = Self::insert_roots(&mut tx, id, roots).await?;
        sqlx::query("UPDATE projects SET primary_root_id=? WHERE id=?")
            .bind(&ids[primary])
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let project = self
            .load_project(&mut tx, id)
            .await?
            .ok_or_else(|| invalid("missing project"))?;
        tx.commit().await?;
        Ok(ProjectCreate::Created(project))
    }

    /// The project a create request made, with the digest of that request's
    /// input, so a retry can be answered before any side effect.
    pub async fn project_creation(
        &self,
        request_id: &str,
    ) -> Result<Option<(StoredProject, Option<String>)>, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        let Some(row) = sqlx::query(
            "SELECT id,creation_payload_sha256 FROM projects WHERE creation_request_id=?",
        )
        .bind(request_id)
        .fetch_optional(&mut *connection)
        .await?
        else {
            return Ok(None);
        };
        let id: String = row.get("id");
        let payload = row.get::<Option<String>, _>("creation_payload_sha256");
        Ok(self
            .load_project(&mut connection, &id)
            .await?
            .map(|project| (project, payload)))
    }

    pub async fn project(&self, id: &str) -> Result<Option<StoredProject>, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        let project = self.load_project(&mut connection, id).await?;
        self.with_auto_archive(&mut connection, project).await
    }

    pub async fn list_projects(&self) -> Result<Vec<StoredProject>, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM projects ORDER BY pin_order IS NULL, pin_order, COALESCE(last_used_at, created_at) DESC, id")
            .fetch_all(&mut *connection)
            .await?;
        let mut projects = Vec::with_capacity(ids.len());
        for id in ids {
            let project = self.load_project(&mut connection, &id).await?;
            if let Some(project) = self.with_auto_archive(&mut connection, project).await? {
                projects.push(project);
            }
        }
        Ok(projects)
    }

    /// Renames only while the stored name is still `expected`, in one write, so
    /// a name the owner chose in between is never overwritten. Returns whether
    /// this call renamed it.
    pub async fn rename_project_if_named(
        &self,
        id: &str,
        expected: &str,
        name: &str,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(invalid(
                "Project names must be between 1 and 120 characters",
            ));
        }
        let updated = sqlx::query("UPDATE projects SET name=?, updated_at=? WHERE id=? AND name=?")
            .bind(name)
            .bind(now)
            .bind(id)
            .bind(expected)
            .execute(&self.pool)
            .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Name, visibility and pin changes never touch folders or provider state.
    pub async fn update_project_metadata(
        &self,
        id: &str,
        name: Option<&str>,
        included: Option<bool>,
        pinned: Option<bool>,
        now: &str,
    ) -> Result<Option<StoredProject>, sqlx::Error> {
        if let Some(name) = name {
            let name = name.trim();
            if name.is_empty() || name.chars().count() > 120 {
                return Err(invalid(
                    "Project names must be between 1 and 120 characters",
                ));
            }
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query("UPDATE projects SET name=COALESCE(?,name), is_included=COALESCE(?,is_included), updated_at=? WHERE id=?")
            .bind(name.map(str::trim)).bind(included.map(i64::from)).bind(now).bind(id)
            .execute(&mut *tx).await?;
        if updated.rows_affected() == 0 {
            return Ok(None);
        }
        match pinned {
            Some(true) => {
                sqlx::query("UPDATE projects SET pin_order=(SELECT COALESCE(MAX(pin_order),0)+1 FROM projects) WHERE id=? AND pin_order IS NULL")
                    .bind(id).execute(&mut *tx).await?;
            }
            Some(false) => {
                sqlx::query("UPDATE projects SET pin_order=NULL WHERE id=?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {}
        }
        let project = self.load_project(&mut tx, id).await?;
        let project = self.with_auto_archive(&mut tx, project).await?;
        tx.commit().await?;
        Ok(project)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Replaces folders when the caller saw the current revision. Existing
    /// conversations keep their recorded cwd; dispatch revalidates membership.
    pub async fn update_project_roots(
        &self,
        id: &str,
        expected_revision: i64,
        roots: &[ProjectRootInput],
        primary: usize,
        now: &str,
    ) -> Result<Option<StoredProject>, sqlx::Error> {
        self.update_project_roots_and_rule(id, expected_revision, roots, primary, None, now)
            .await
    }

    /// Replaces folders and, when `rule` is set, the Project's own
    /// auto-archive rule in one transaction: a stale revision or an invalid
    /// rule leaves both unchanged.
    pub async fn update_project_roots_and_rule(
        &self,
        id: &str,
        expected_revision: i64,
        roots: &[ProjectRootInput],
        primary: usize,
        rule: Option<Option<(i64, bool)>>,
        now: &str,
    ) -> Result<Option<StoredProject>, sqlx::Error> {
        validate_roots(roots, primary)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = self.load_project(&mut tx, id).await? else {
            return Ok(None);
        };
        if current.roots_revision != expected_revision {
            return Err(invalid("roots_revision_changed"));
        }
        if let Some(rule) = rule {
            write_project_auto_archive_rule(&mut tx, id, rule, now).await?;
        }
        // Keep IDs for unchanged folders so references remain stable.
        let mut ids = Vec::with_capacity(roots.len());
        sqlx::query("UPDATE project_roots SET ordinal=-1-ordinal WHERE project_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (ordinal, root) in roots.iter().enumerate() {
            let existing = current
                .roots
                .iter()
                .find(|r| r.canonical_path == root.canonical_path);
            let root_id = existing
                .map(|r| r.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            if existing.is_some() {
                sqlx::query("UPDATE project_roots SET path=?,ordinal=? WHERE id=?")
                    .bind(&root.path)
                    .bind(ordinal as i64)
                    .bind(&root_id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("INSERT INTO project_roots(id,project_id,path,canonical_path,ordinal) VALUES(?,?,?,?,?)")
                    .bind(&root_id).bind(id).bind(&root.path).bind(&root.canonical_path).bind(ordinal as i64)
                    .execute(&mut *tx).await?;
            }
            ids.push(root_id);
        }
        sqlx::query("DELETE FROM project_roots WHERE project_id=? AND ordinal<0")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE projects SET primary_root_id=?,roots_revision=roots_revision+1,updated_at=? WHERE id=?")
            .bind(&ids[primary]).bind(now).bind(id).execute(&mut *tx).await?;
        let project = self.load_project(&mut tx, id).await?;
        tx.commit().await?;
        Ok(project)
    }

    pub async fn touch_project(
        &self,
        id: &str,
        family: AgentFamily,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE projects SET last_family=?,last_used_at=? WHERE id=?")
            .bind(family.as_str())
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn project_provider_ref(
        &self,
        project: &str,
        family: AgentFamily,
        store: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT provider_project_id FROM project_provider_refs WHERE project_id=? AND agent_family=? AND provider_store=?")
            .bind(project).bind(family.as_str()).bind(store)
            .fetch_optional(&self.pool).await
    }

    /// First writer wins so a late duplicate creation cannot retarget a project.
    pub async fn set_project_provider_ref(
        &self,
        project: &str,
        family: AgentFamily,
        store: &str,
        provider_project_id: &str,
    ) -> Result<String, sqlx::Error> {
        sqlx::query("INSERT INTO project_provider_refs(project_id,agent_family,provider_store,provider_project_id) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
            .bind(project).bind(family.as_str()).bind(store).bind(provider_project_id)
            .execute(&self.pool).await?;
        self.project_provider_ref(project, family, store)
            .await?
            .ok_or_else(|| invalid("missing provider project"))
    }

    /// Replace only the stale reference this caller checked. A concurrent
    /// repair keeps its choice; callers must validate the returned winner.
    pub async fn replace_project_provider_ref(
        &self,
        project: &str,
        family: AgentFamily,
        store: &str,
        expected: &str,
        provider_project_id: &str,
    ) -> Result<String, sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE project_provider_refs SET provider_project_id=? WHERE project_id=? AND agent_family=? AND provider_store=? AND provider_project_id=?")
            .bind(provider_project_id).bind(project).bind(family.as_str()).bind(store).bind(expected)
            .execute(&mut *tx).await?;
        let winner = sqlx::query_scalar("SELECT provider_project_id FROM project_provider_refs WHERE project_id=? AND agent_family=? AND provider_store=?")
            .bind(project).bind(family.as_str()).bind(store).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(winner)
    }

    /// The immutable request payload is read before current runtime/folder
    /// gates so an already accepted creation can replay its original receipt.
    pub async fn project_creation_by_request(
        &self,
        request_id: &str,
    ) -> Result<Option<(StoredProjectConversation, serde_json::Value)>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM project_conversations WHERE creation_request_id=?")
            .persistent(false)
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            let payload: Option<String> = row.get("creation_payload");
            let payload = payload
                .and_then(|value| serde_json::from_str(&value).ok())
                .ok_or_else(|| invalid("creation_payload_unavailable"))?;
            Ok((project_conversation(&row)?, payload))
        })
        .transpose()
    }

    /// Idempotent by creation request and by native identity: rediscovering a
    /// session returns its existing conversation instead of a duplicate.
    pub async fn create_project_conversation(
        &self,
        insert: ProjectConversationInsert<'_>,
    ) -> Result<ProjectConversationCreate, sqlx::Error> {
        self.create_project_conversation_inner(insert, None, None)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn create_project_conversation_with_request_digest(
        &self,
        insert: ProjectConversationInsert<'_>,
        request_digest: &str,
    ) -> Result<ProjectConversationCreate, sqlx::Error> {
        self.create_project_conversation_inner(insert, Some(request_digest), None)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    /// With `delegation`, the parent's child limit is checked and the
    /// delegation recorded in the conversation's own transaction; None means
    /// the parent is at its limit and nothing was created.
    pub(crate) async fn create_project_conversation_inner(
        &self,
        insert: ProjectConversationInsert<'_>,
        request_digest: Option<&str>,
        delegation: Option<&crate::thread_tools::DelegationReservation<'_>>,
    ) -> Result<Option<ProjectConversationCreate>, sqlx::Error> {
        if !ACCESS_MODES.contains(&insert.access_mode) {
            return Err(invalid("Unknown project access mode"));
        }
        if !insert.family.provider().capabilities.access_choices
            && insert.access_mode != "workspace"
        {
            return Err(invalid(
                "Change permissions in OpenCode settings on your Mac.",
            ));
        }
        if !AgentFamily::is_known_approval_mode(insert.claude_approval) {
            return Err(invalid("Unknown Claude approval mode"));
        }
        // OpenCode and Cursor store only the neutral value, which stands for
        // their own settings (`PermissionProfile::Native`), never an Ask.
        if insert
            .family
            .permission_profile(Some(insert.claude_approval))
            .is_none()
        {
            return Err(invalid("Claude approval applies only to Claude threads"));
        }
        if insert.title.trim().is_empty() || insert.title.len() > 400 {
            return Err(invalid("Invalid conversation title"));
        }
        let creation_payload = project_creation_payload(&insert, request_digest);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(request) = insert.creation_request_id {
            if let Some(row) =
                sqlx::query("SELECT * FROM project_conversations WHERE creation_request_id=?")
                    .persistent(false)
                    .bind(request)
                    .fetch_optional(&mut *tx)
                    .await?
            {
                // Compare the original request, never settings changed by PATCH.
                let existing = matching_project_creation(&row, &creation_payload)?;
                if let Some(delegation) = delegation {
                    if !delegation
                        .reserve(&mut tx, &existing.conversation_id)
                        .await?
                    {
                        return Ok(None);
                    }
                }
                tx.commit().await?;
                return Ok(Some(ProjectConversationCreate::Existing(existing)));
            }
        }
        if let Some(native) = insert.native_session_id {
            if let Some(row) = sqlx::query("SELECT * FROM project_conversations WHERE agent_family=? AND provider_store=? AND native_session_id=?")
                .bind(insert.family.as_str()).bind(insert.provider_store).bind(native)
                .fetch_optional(&mut *tx).await?
            {
                tx.commit().await?;
                return Ok(Some(ProjectConversationCreate::Existing(project_conversation(&row)?)));
            }
        }
        let owned: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM subagent_ownership WHERE conversation_id=?) OR EXISTS(SELECT 1 FROM runtime_bindings WHERE conversation_id=?)")
            .bind(insert.conversation_id).bind(insert.conversation_id).fetch_one(&mut *tx).await?;
        if owned != 0 {
            return Err(invalid("conversation_already_owned"));
        }
        if let Some(delegation) = delegation {
            if !delegation.has_room(&mut tx).await? {
                return Ok(None);
            }
        }
        sqlx::query("INSERT INTO conversations(id,codex_thread_id,session_id,created_at) VALUES(?,NULL,NULL,?)")
            .bind(insert.conversation_id).bind(insert.now).execute(&mut *tx).await?;
        // A new title counts as chosen (the column's default) until its
        // creator marks it taken (`mark_project_conversation_title_taken`).
        sqlx::query("INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,native_session_id,cwd,roots_revision,title,model,effort,service_tier,access_mode,claude_approval,plan_mode,creation_request_id,creation_payload,created_at,updated_at,last_activity_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(insert.conversation_id).bind(insert.project_id).bind(insert.family.as_str())
            .bind(insert.provider_store).bind(insert.native_session_id).bind(insert.cwd)
            .bind(insert.roots_revision).bind(insert.title.trim()).bind(insert.model).bind(insert.effort).bind(insert.service_tier)
            .bind(insert.access_mode).bind(insert.claude_approval).bind(i64::from(insert.plan_mode))
            .bind(insert.creation_request_id).bind(insert.creation_request_id.map(|_| creation_payload.to_string()))
            .bind(insert.now).bind(insert.now).bind(insert.now)
            .execute(&mut *tx).await?;
        let row = sqlx::query("SELECT * FROM project_conversations WHERE conversation_id=?")
            .bind(insert.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
        let created = project_conversation(&row)?;
        if let Some(delegation) = delegation {
            if !delegation
                .reserve(&mut tx, &created.conversation_id)
                .await?
            {
                return Ok(None);
            }
        }
        tx.commit().await?;
        Ok(Some(ProjectConversationCreate::Created(created)))
    }

    pub async fn project_conversation(
        &self,
        conversation: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        sqlx::query("SELECT * FROM project_conversations WHERE conversation_id=?")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(project_conversation)
            .transpose()
    }

    /// Points a chat at another folder (its new worktree); history and
    /// provider session stay.
    pub async fn move_project_conversation_folder(
        &self,
        conversation: &str,
        cwd: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT OR REPLACE INTO project_folder_move_guard(conversation_id) VALUES(?)")
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE project_conversations SET cwd=?, updated_at=? WHERE conversation_id=?")
            .bind(cwd)
            .bind(now)
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM project_folder_move_guard WHERE conversation_id=?")
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    /// Records that a new thread's title was taken from its first message or
    /// a provider's name, so lists may name it by the same rule later.
    pub async fn mark_project_conversation_title_taken(
        &self,
        conversation: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE project_conversations SET title_chosen=0 WHERE conversation_id=?")
            .bind(conversation)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// Names a thread whose taken title says nothing yet, the first time its
    /// provider names it: only while the title is still taken and still reads
    /// `from`. Returns false when it is chosen or changed meanwhile.
    pub async fn name_untitled_project_conversation(
        &self,
        conversation: &str,
        from: &str,
        to: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE project_conversations SET title=? WHERE conversation_id=? AND title=? AND title_chosen=0")
            .bind(to)
            .bind(conversation)
            .bind(from)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Names a thread T3 Code has by T3 Code's title `title`, unless that is
    /// the title Cupola last took from T3 Code: a rename in Cupola stands
    /// until T3 Code's title changes again. Returns whether it was taken.
    pub async fn follow_t3_code_title(
        &self,
        conversation: &str,
        title: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE project_conversations SET title=?1,title_chosen=1,t3_title=?1 WHERE conversation_id=?2 AND t3_title IS NOT ?1")
            .bind(title)
            .bind(conversation)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Hides or restores a thread in Wonder only. Returns false when the
    /// conversation does not exist.
    pub async fn set_project_conversation_wonder_archived(
        &self,
        conversation: &str,
        archived: bool,
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE project_conversations SET wonder_archived_at=CASE WHEN ? THEN COALESCE(wonder_archived_at, ?) ELSE NULL END, updated_at=? WHERE conversation_id=?",
        )
        .bind(archived)
        .bind(now)
        .bind(now)
        .bind(conversation)
        .execute(&mut *tx)
        .await?;
        if archived {
            drop_usage_resume(&mut tx, conversation).await?;
        }
        tx.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Every thread restored in Cupola while its home app keeps it settled,
    /// snoozed or archived, with the closing it was restored from.
    pub async fn kept_open_elsewhere(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT conversation_id,marker FROM kept_open_elsewhere")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// Shows the thread in Cupola although its home app closed it (`marker`).
    pub async fn keep_open_elsewhere(
        &self,
        conversation: &str,
        marker: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO kept_open_elsewhere(conversation_id,marker) VALUES(?,?) ON CONFLICT(conversation_id) DO UPDATE SET marker=excluded.marker")
            .bind(conversation)
            .bind(marker)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn project_conversation_by_native(
        &self,
        family: AgentFamily,
        store: &str,
        native: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        sqlx::query("SELECT * FROM project_conversations WHERE agent_family=? AND provider_store=? AND native_session_id=?")
            .bind(family.as_str()).bind(store).bind(native)
            .fetch_optional(&self.pool).await?
            .as_ref().map(project_conversation).transpose()
    }

    /// Resolve a provider page in one snapshot, preserving current-session
    /// precedence over the most recent thread that left a session behind.
    pub async fn project_conversation_owners(
        &self,
        keys: &[(AgentFamily, String, String)],
    ) -> Result<Vec<NativeConversationOwner>, sqlx::Error> {
        let keys = serde_json::to_string(&keys).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let rows = sqlx::query("WITH requested AS (SELECT json_extract(value,'$[0]') AS family,json_extract(value,'$[1]') AS store,json_extract(value,'$[2]') AS native FROM json_each(?)) SELECT c.*,k.family AS requested_family,k.store AS requested_store,k.native AS requested_native,1 AS is_current FROM requested k JOIN project_conversations c ON c.agent_family=k.family AND c.provider_store=k.store AND c.native_session_id=k.native UNION ALL SELECT c.*,k.family AS requested_family,k.store AS requested_store,k.native AS requested_native,0 AS is_current FROM requested k JOIN project_runtime_history h ON h.id=(SELECT MAX(id) FROM project_runtime_history WHERE agent_family=k.family AND provider_store=k.store AND native_session_id=k.native) JOIN project_conversations c ON c.conversation_id=h.conversation_id WHERE NOT EXISTS(SELECT 1 FROM project_conversations current WHERE current.agent_family=k.family AND current.provider_store=k.store AND current.native_session_id=k.native)")
            .bind(keys).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(NativeConversationOwner {
                    family: family(row.get::<&str, _>("requested_family"))?,
                    provider_store: row.get("requested_store"),
                    native_session_id: row.get("requested_native"),
                    conversation: project_conversation(row)?,
                    is_current: row.get("is_current"),
                })
            })
            .collect()
    }

    /// Revision of Project membership/archive/folder visibility, not activity.
    pub async fn project_visibility_revision(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT revision FROM project_visibility_revision WHERE id=1")
            .fetch_one(&self.pool)
            .await
    }

    /// Only the IDs and folders needed for Live Activity visibility. Avoid
    /// materializing archived chats or their presentation/settings columns.
    pub async fn visible_project_conversations(
        &self,
        project: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT conversation_id,cwd FROM project_conversations WHERE project_id=? AND wonder_archived_at IS NULL")
            .bind(project).fetch_all(&self.pool).await
    }

    pub async fn project_conversations(
        &self,
        project: &str,
    ) -> Result<Vec<StoredProjectConversation>, sqlx::Error> {
        sqlx::query("SELECT * FROM project_conversations WHERE project_id=? ORDER BY is_pinned DESC, last_activity_at DESC, conversation_id")
            .bind(project)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(project_conversation)
            .collect()
    }

    /// Remembers which conversation a fork was copied from and where it was cut.
    pub async fn record_project_fork(
        &self,
        conversation: &str,
        source: &str,
        at_turn: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO project_conversation_forks(conversation_id,forked_from,forked_at_turn) VALUES(?,?,?)")
            .bind(conversation).bind(source).bind(at_turn)
            .execute(&self.pool).await?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    /// The conversation a fork came from and the turn it was cut at.
    pub async fn project_fork_source(
        &self,
        conversation: &str,
    ) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
        let row = sqlx::query("SELECT forked_from,forked_at_turn FROM project_conversation_forks WHERE conversation_id=?")
            .bind(conversation).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| (row.get("forked_from"), row.get("forked_at_turn"))))
    }

    /// Records the provider's actual session exactly once.
    pub async fn set_project_native_session(
        &self,
        conversation: &str,
        native: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let updated = sqlx::query("UPDATE project_conversations SET native_session_id=?,updated_at=? WHERE conversation_id=? AND (native_session_id IS NULL OR native_session_id=?)")
            .bind(native).bind(now).bind(conversation).bind(native)
            .execute(&self.pool).await?;
        if updated.rows_affected() != 1 {
            return Err(invalid(
                "The conversation already has another native session",
            ));
        }
        Ok(())
    }

    pub async fn update_project_conversation(
        &self,
        conversation: &str,
        patch: ProjectConversationPatch<'_>,
        now: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        self.update_project_conversation_with_expected_settings(conversation, patch, None, now)
            .await
    }

    /// Reject a settings PATCH if another device changed model, effort, or
    /// speed after the caller validated their combined values.
    pub async fn update_project_conversation_if_settings_match(
        &self,
        conversation: &str,
        patch: ProjectConversationPatch<'_>,
        expected_model: Option<&str>,
        expected_effort: Option<&str>,
        expected_tier: Option<&str>,
        now: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        self.update_project_conversation_with_expected_settings(
            conversation,
            patch,
            Some((expected_model, expected_effort, expected_tier)),
            now,
        )
        .await
    }

    async fn update_project_conversation_with_expected_settings(
        &self,
        conversation: &str,
        patch: ProjectConversationPatch<'_>,
        expected: Option<(Option<&str>, Option<&str>, Option<&str>)>,
        now: &str,
    ) -> Result<Option<StoredProjectConversation>, sqlx::Error> {
        if patch
            .access_mode
            .is_some_and(|mode| !ACCESS_MODES.contains(&mode))
        {
            return Err(invalid("Unknown project access mode"));
        }
        if patch
            .claude_approval
            .is_some_and(|mode| !AgentFamily::is_known_approval_mode(mode))
        {
            return Err(invalid("Unknown Claude approval mode"));
        }
        if patch
            .title
            .is_some_and(|title| title.trim().is_empty() || title.len() > 400)
        {
            return Err(invalid("Invalid conversation title"));
        }
        if patch.claude_approval.is_some() || patch.access_mode.is_some() {
            // A thread's family never changes, so this check cannot race.
            let family: Option<String> = sqlx::query_scalar(
                "SELECT agent_family FROM project_conversations WHERE conversation_id=?",
            )
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(family) = family.as_deref().map(self::family).transpose()? {
                if !family.provider().capabilities.access_choices {
                    return Err(invalid(
                        "Change permissions in OpenCode settings on your Mac.",
                    ));
                }
                if patch.claude_approval.is_some() && !family.allows_approval_choice() {
                    return Err(invalid("Claude approval applies only to Claude threads"));
                }
            }
        }
        let updated = sqlx::query("UPDATE project_conversations SET title=COALESCE(?,title),title_chosen=CASE WHEN ? IS NULL THEN title_chosen ELSE 1 END,is_pinned=COALESCE(?,is_pinned),has_unread=COALESCE(?,has_unread),model=COALESCE(?,model),effort=CASE WHEN ? THEN ? ELSE effort END,service_tier=CASE WHEN ? THEN ? ELSE service_tier END,access_mode=COALESCE(?,access_mode),claude_approval=COALESCE(?,claude_approval),plan_mode=COALESCE(?,plan_mode),native_settings_turn=COALESCE(?,native_settings_turn),updated_at=? WHERE conversation_id=? AND (?=0 OR (model IS ? AND effort IS ? AND service_tier IS ?))")
            .bind(patch.title.map(str::trim)).bind(patch.title.is_some().then_some(1_i64)).bind(patch.pinned.map(i64::from)).bind(patch.unread.map(i64::from))
            .bind(patch.model).bind(patch.effort.is_some()).bind(patch.effort.flatten())
            .bind(patch.service_tier.is_some()).bind(patch.service_tier.flatten()).bind(patch.access_mode)
            .bind(patch.claude_approval).bind(patch.plan_mode.map(i64::from))
            .bind(patch.native_settings_turn)
            .bind(now).bind(conversation).bind(expected.is_some())
            .bind(expected.and_then(|value| value.0))
            .bind(expected.and_then(|value| value.1))
            .bind(expected.and_then(|value| value.2))
            .execute(&self.pool).await?;
        if updated.rows_affected() == 0 {
            return if expected.is_some() && self.project_conversation(conversation).await?.is_some()
            {
                Err(invalid("project_settings_changed"))
            } else {
                Ok(None)
            };
        }
        self.project_conversation(conversation).await
    }

    /// Native turn ids Wonder itself started in this conversation.
    pub async fn wonder_turn_ids(&self, conversation: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT codex_turn_id FROM messages WHERE conversation_id=? AND codex_turn_id IS NOT NULL")
            .bind(conversation).fetch_all(&self.pool).await
    }

    /// The settings accepted with this message, independent of later edits.
    /// Pre-upgrade messages without a snapshot use current thread settings.
    pub async fn project_message_execution_settings(
        &self,
        message: &str,
    ) -> Result<Option<(Option<String>, Option<String>, Option<String>)>, sqlx::Error> {
        let row = sqlx::query("SELECT model,reasoning_effort,service_tier FROM message_execution_settings WHERE message_id=?")
            .bind(message).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| {
            (
                row.get("model"),
                row.get("reasoning_effort"),
                row.get("service_tier"),
            )
        }))
    }

    /// Attached, pinned conversations of included projects, newest activity first.
    pub async fn pinned_project_conversations(
        &self,
        limit: i64,
    ) -> Result<Vec<StoredProjectConversation>, sqlx::Error> {
        sqlx::query("SELECT c.* FROM project_conversations c JOIN projects p ON p.id=c.project_id WHERE c.is_pinned=1 AND p.is_included=1 ORDER BY c.last_activity_at DESC, c.conversation_id LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(project_conversation)
            .collect()
    }

    pub async fn touch_project_conversation(
        &self,
        conversation: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE project_conversations SET last_activity_at=? WHERE conversation_id=?")
            .bind(now)
            .bind(conversation)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A Project binding is created once and cannot adopt another owned session.
    pub async fn bind_project_runtime(
        &self,
        conversation: &str,
        family: AgentFamily,
        provider_store: &str,
        thread: &str,
        session: Option<&str>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        if thread.trim().is_empty() || thread.len() > 512 || !family.accepts_thread_id(thread) {
            return Err(invalid("Invalid provider session identity"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owner: Option<(String, String)> = sqlx::query_as(
            "SELECT agent_family,provider_store FROM project_conversations WHERE conversation_id=?",
        )
        .bind(conversation)
        .fetch_optional(&mut *tx)
        .await?;
        if owner.as_ref().map(|(f, s)| (f.as_str(), s.as_str()))
            != Some((family.as_str(), provider_store))
        {
            return Err(invalid(
                "The project conversation belongs to another agent family",
            ));
        }
        if let Some(existing) = sqlx::query("SELECT runtime_thread_id,execution_scope FROM runtime_bindings WHERE conversation_id=?")
            .bind(conversation).fetch_optional(&mut *tx).await?
        {
            if existing.get::<String, _>("execution_scope") != EXECUTION_SCOPE_PROJECTS
                || existing.get::<String, _>("runtime_thread_id") != thread
            {
                return Err(invalid("The project conversation is bound to another session"));
            }
        }
        let foreign: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE runtime_thread_id=? AND conversation_id<>?)")
            .bind(thread).bind(conversation).fetch_one(&mut *tx).await?;
        if foreign != 0 {
            return Err(invalid(
                "The native session already belongs to another conversation",
            ));
        }
        sqlx::query("UPDATE conversations SET codex_thread_id=?,session_id=COALESCE(?,session_id) WHERE id=?")
            .bind(thread).bind(session).bind(conversation).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO runtime_bindings(conversation_id,agent_family,runtime_thread_id,session_id,updated_at,execution_scope,provider_store) VALUES(?,?,?,?,?,'projects',?) ON CONFLICT(conversation_id) DO UPDATE SET session_id=COALESCE(excluded.session_id,runtime_bindings.session_id),updated_at=excluded.updated_at")
            .bind(conversation).bind(family.as_str()).bind(thread).bind(session).bind(now).bind(provider_store)
            .execute(&mut *tx).await?;
        tx.commit().await
    }

    /// Family and execution scope for a transport thread, if Wonder owns it.
    /// The family, execution scope and provider store (the Project thread's
    /// home; `None` for a binding without a Project thread) of the
    /// conversation bound to `thread`.
    pub async fn runtime_route_for_thread(
        &self,
        thread: &str,
    ) -> Result<Option<(AgentFamily, String, Option<String>)>, sqlx::Error> {
        let row = sqlx::query("SELECT b.agent_family,b.execution_scope,p.provider_store FROM runtime_bindings b LEFT JOIN project_conversations p ON p.conversation_id=b.conversation_id WHERE b.runtime_thread_id=? LIMIT 1")
            .bind(thread).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok((
                family(row.get("agent_family"))?,
                row.get("execution_scope"),
                row.get("provider_store"),
            ))
        })
        .transpose()
    }

    /// Conversations that read as working: Cupola-owned unfinished work
    /// (queued, in flight or running) or an unconfirmed delivery. Queued work
    /// counts on purpose, so Mac dialogs and computer push treat a
    /// conversation with a waiting message as running.
    pub async fn running_conversations(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(sql!(
            "SELECT DISTINCT m.conversation_id FROM messages m WHERE {} ORDER BY m.conversation_id",
            crate::working_message_sql("m")
        ))
        .fetch_all(&self.pool)
        .await
    }

    pub async fn conversation_has_active_turn(
        &self,
        conversation: &str,
    ) -> Result<bool, sqlx::Error> {
        let active: i64 = sqlx::query_scalar(sql!(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ({}))",
            turn_states_sql(TurnScope::Admitted)
        ))
        .bind(conversation)
        .fetch_one(&self.pool)
        .await?;
        Ok(active != 0)
    }

    /// Whether a reply Cupola itself runs may be writing this conversation's
    /// transcript: `conversation_has_active_turn`, except for a message being
    /// sent into another app (T3 Code's `t3_thread_send`). That app runs the
    /// turn and may write its reply before the send is answered, so the
    /// reply must not be mistaken for Cupola's own.
    pub async fn conversation_has_own_active_turn(
        &self,
        conversation: &str,
    ) -> Result<bool, sqlx::Error> {
        let active: i64 = sqlx::query_scalar(sql!("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ({}) AND NOT EXISTS(SELECT 1 FROM dispatch_attempts d WHERE d.id=(SELECT MAX(id) FROM dispatch_attempts WHERE message_id=messages.id) AND json_extract(d.context_json,'$.deliveryMethod')='t3_thread_send'))", turn_states_sql(TurnScope::Admitted)))
            .bind(conversation).fetch_one(&self.pool).await?;
        Ok(active != 0)
    }

    /// Turns Cupola submitted here that may still be running.
    pub async fn conversation_unsettled_turns(
        &self,
        conversation: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(sql!("SELECT DISTINCT codex_turn_id FROM messages WHERE conversation_id=? AND codex_turn_id IS NOT NULL AND state IN ({}) ORDER BY codex_turn_id", turn_states_sql(TurnScope::Settleable)))
            .bind(conversation).fetch_all(&self.pool).await
    }

    /// Archive is never refused: hide the thread in Cupola and settle its
    /// unfinished work in one step. Queued, in-flight and uncertain sends end
    /// interrupted, so nothing dispatches to or waits on an archived thread.
    /// A send still being submitted (`submitting`) keeps its receipt's
    /// authority. Returns the settled message IDs.
    pub async fn archive_project_conversation(
        &self,
        conversation: &str,
        submitting: &[String],
        now: &str,
    ) -> Result<Option<Vec<String>>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let submitting =
            serde_json::to_string(submitting).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let settled: Vec<String> = sqlx::query_scalar(sql!("UPDATE messages SET state='interrupted',queue_revision=queue_revision+1 WHERE conversation_id=? AND state IN ({}) AND id NOT IN (SELECT value FROM json_each(?)) RETURNING id", turn_states_sql(TurnScope::Held)))
            .bind(conversation).bind(&submitting).fetch_all(&mut *tx).await?;
        sqlx::query("UPDATE dispatch_attempts SET phase='interrupted' WHERE message_id IN (SELECT value FROM json_each(?)) AND phase IN ('claimed','submitting','dispatching_to_codex','accepted_by_codex','streaming','uncertain')")
            .bind(serde_json::to_string(&settled).map_err(|e| sqlx::Error::Protocol(e.to_string()))?)
            .execute(&mut *tx).await?;
        sqlx::query(sql!("UPDATE assistant_messages SET state='failed',updated_at=? WHERE conversation_id=? AND state IN ({})", turn_states_sql(TurnScope::Running)))
            .bind(now).bind(conversation).execute(&mut *tx).await?;
        let updated = sqlx::query("UPDATE project_conversations SET wonder_archived_at=COALESCE(wonder_archived_at, ?), updated_at=? WHERE conversation_id=?")
            .bind(now).bind(now).bind(conversation).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Ok(None);
        }
        drop_usage_resume(&mut tx, conversation).await?;
        tx.commit().await?;
        Ok(Some(settled))
    }

    /// Auto-archive's commit: hide the thread only if nothing changed since
    /// the sweep judged it idle. One statement, so a send, pin or approval
    /// arriving in between wins and the thread stays. `last_activity_at` and
    /// `last_sent` are the values the sweep saw; `threads` are the provider
    /// threads whose approvals would hold it. Whether the thread was archived.
    pub async fn auto_archive_project_conversation(
        &self,
        conversation: &str,
        last_activity_at: &str,
        last_sent: Option<&str>,
        threads: &[String],
        now: &str,
    ) -> Result<bool, sqlx::Error> {
        let threads =
            serde_json::to_string(threads).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(sql!("UPDATE project_conversations SET wonder_archived_at=?, updated_at=? WHERE conversation_id=? AND wonder_archived_at IS NULL AND is_pinned=0 AND last_activity_at=? AND (SELECT MAX(created_at) FROM messages WHERE conversation_id=?) IS ? AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id=? AND state IN ({})) AND NOT EXISTS(SELECT 1 FROM approvals WHERE state IN ('pending','resolving') AND thread_id IN (SELECT value FROM json_each(?)))", turn_states_sql(TurnScope::Held)))
            .bind(now).bind(now).bind(conversation).bind(last_activity_at)
            .bind(conversation).bind(last_sent).bind(conversation).bind(&threads)
            .execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Ok(false);
        }
        drop_usage_resume(&mut tx, conversation).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn conversation_has_archive_blocking_work(
        &self,
        conversation: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(sql!("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND state IN ({})) OR EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=?1)", turn_states_sql(TurnScope::Held)))
            .bind(conversation).fetch_one(&self.pool).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_folders_may_be_unix_or_windows_drive_paths() {
        use super::is_absolute_path;
        for ok in ["/Users/a/Work", "C:\\Users\\a\\Work", "d:/work", "C:\\"] {
            assert!(is_absolute_path(ok), "{ok}");
        }
        for bad in [
            "",
            "Work",
            "C:Work",
            "\\\\server\\share",
            "\\\\?\\C:\\Work",
            "C:",
            "relative/C:/x",
        ] {
            assert!(!is_absolute_path(bad), "{bad}");
        }
    }

    use super::*;

    fn roots(paths: &[&str]) -> Vec<ProjectRootInput> {
        paths
            .iter()
            .map(|path| ProjectRootInput {
                path: (*path).into(),
                canonical_path: (*path).into(),
            })
            .collect()
    }

    async fn project(store: &Store, id: &str, paths: &[&str]) -> StoredProject {
        match store
            .create_project(
                id,
                &format!("request-{id}"),
                "hash",
                "App",
                &roots(paths),
                0,
                "now",
            )
            .await
            .unwrap()
        {
            ProjectCreate::Created(project) => project,
            other => panic!("{other:?}"),
        }
    }

    fn conversation<'a>(
        id: &'a str,
        project: &'a str,
        native: Option<&'a str>,
        request: Option<&'a str>,
    ) -> ProjectConversationInsert<'a> {
        ProjectConversationInsert {
            conversation_id: id,
            project_id: project,
            family: AgentFamily::Codex,
            provider_store: "codex:home",
            native_session_id: native,
            cwd: "/work/app",
            roots_revision: 1,
            title: "Fix",
            model: None,
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: request,
            now: "now",
        }
    }

    // Contract: Full access no longer keeps a command sandbox. Upgrading must
    // not widen a Claude thread that chose Full access with sandboxed commands:
    // it moves to Auto; an explicit opt-out and every other thread keep their modes.
    #[tokio::test]
    async fn only_the_guarded_folder_move_may_change_a_chat_folder() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        store
            .create_project_conversation(conversation("c1", "p1", None, None))
            .await
            .unwrap();
        assert!(sqlx::query(
            "UPDATE project_conversations SET cwd='/x' WHERE conversation_id='c1'"
        )
        .execute(&store.pool)
        .await
        .is_err());
        store
            .move_project_conversation_folder("c1", "/work/app-wt", "later")
            .await
            .unwrap();
        let moved = store.project_conversation("c1").await.unwrap().unwrap();
        assert_eq!(moved.cwd, "/work/app-wt");
        assert!(sqlx::query(
            "UPDATE project_conversations SET cwd='/y' WHERE conversation_id='c1'"
        )
        .execute(&store.pool)
        .await
        .is_err());
    }

    #[tokio::test]
    async fn visibility_revision_ignores_activity_but_tracks_membership_and_archives() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        store
            .create_project_conversation(conversation("c1", "p1", None, None))
            .await
            .unwrap();
        let revision = store.project_visibility_revision().await.unwrap();
        assert_eq!(
            store
                .visible_project_conversations("p1")
                .await
                .unwrap()
                .len(),
            1
        );
        sqlx::query("UPDATE project_conversations SET title='new title',last_activity_at='later',has_unread=1 WHERE conversation_id='c1'").execute(&store.pool).await.unwrap();
        assert_eq!(store.project_visibility_revision().await.unwrap(), revision);
        store
            .set_project_conversation_wonder_archived("c1", true, "now")
            .await
            .unwrap();
        assert!(store.project_visibility_revision().await.unwrap() > revision);
        assert!(store
            .visible_project_conversations("p1")
            .await
            .unwrap()
            .is_empty());
        let revision = store.project_visibility_revision().await.unwrap();
        store.set_include_worktrees(false).await.unwrap();
        assert!(store.project_visibility_revision().await.unwrap() > revision);
        let revision = store.project_visibility_revision().await.unwrap();
        store.set_include_worktrees(false).await.unwrap();
        assert_eq!(store.project_visibility_revision().await.unwrap(), revision);
        sqlx::query("UPDATE project_roots SET canonical_path='/work/other' WHERE project_id='p1'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.project_visibility_revision().await.unwrap() > revision);
    }

    #[tokio::test]
    async fn sandboxed_full_access_threads_move_to_auto_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("prior.sqlite").display()
        );
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let mut prior = sqlx::migrate!();
        prior.migrations = std::borrow::Cow::Owned(
            prior
                .migrations
                .iter()
                .filter(|m| m.version < 90)
                .cloned()
                .collect(),
        );
        prior.run(&pool).await.unwrap();
        let old = Store { pool };
        project(&old, "p1", &["/work/app"]).await;
        for (id, family, access, approval, opted_out) in [
            (
                "sandboxed",
                AgentFamily::Claude,
                "full_access",
                "ask",
                false,
            ),
            ("opted-out", AgentFamily::Claude, "full_access", "ask", true),
            ("asking", AgentFamily::Claude, "workspace", "ask", false),
            ("codex", AgentFamily::Codex, "full_access", "ask", false),
        ] {
            old.create_project_conversation(ProjectConversationInsert {
                family,
                access_mode: access,
                claude_approval: approval,
                ..conversation(id, "p1", None, None)
            })
            .await
            .unwrap();
            sqlx::query(
                "UPDATE project_conversations SET unsandboxed_commands=? WHERE conversation_id=?",
            )
            .bind(i64::from(opted_out))
            .bind(id)
            .execute(&old.pool)
            .await
            .unwrap();
        }
        sqlx::migrate!().run(&old.pool).await.unwrap();
        // Reopen as a relaunch does: the old connection caches `SELECT *`
        // statements with the pre-upgrade column list.
        old.pool.close().await;
        let old = Store {
            pool: sqlx::SqlitePool::connect(&url).await.unwrap(),
        };
        for (id, access, approval) in [
            ("sandboxed", "workspace", "auto"),
            ("opted-out", "full_access", "ask"),
            ("asking", "workspace", "ask"),
            ("codex", "full_access", "ask"),
        ] {
            let stored = old.project_conversation(id).await.unwrap().unwrap();
            assert_eq!(
                (stored.access_mode.as_str(), stored.claude_approval.as_str()),
                (access, approval),
                "{id}"
            );
        }
    }

    // Contract: a stale native-project repair compares the value it checked;
    // neither a duplicate insert nor a late repair can overwrite the winner.
    #[tokio::test]
    async fn provider_project_repairs_preserve_the_concurrent_winner() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "project", &["/work/app"]).await;
        assert_eq!(
            store
                .set_project_provider_ref("project", AgentFamily::Codex, "home", "old")
                .await
                .unwrap(),
            "old"
        );
        assert_eq!(
            store
                .set_project_provider_ref("project", AgentFamily::Codex, "home", "duplicate")
                .await
                .unwrap(),
            "old"
        );
        assert_eq!(
            store
                .replace_project_provider_ref(
                    "project",
                    AgentFamily::Codex,
                    "home",
                    "old",
                    "repaired"
                )
                .await
                .unwrap(),
            "repaired"
        );
        assert_eq!(
            store
                .replace_project_provider_ref("project", AgentFamily::Codex, "home", "old", "late")
                .await
                .unwrap(),
            "repaired"
        );
        assert_eq!(
            store
                .project_provider_ref("project", AgentFamily::Codex, "home")
                .await
                .unwrap()
                .as_deref(),
            Some("repaired")
        );
    }

    // Contract: creation retries converge on one project; a different payload
    // under the same request cannot silently replace the folders.
    #[tokio::test]
    async fn project_creation_is_idempotent_and_validates_folders() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let created = project(&store, "p1", &["/work/app", "/work/docs"]).await;
        assert_eq!(created.primary_root().path, "/work/app");
        assert!(matches!(
            store.create_project("p2", "request-p1", "hash", "App", &roots(&["/work/app"]), 0, "now").await.unwrap(),
            ProjectCreate::Existing(p) if p.id == "p1"
        ));
        assert_eq!(
            store
                .create_project(
                    "p2",
                    "request-p1",
                    "other",
                    "App",
                    &roots(&["/x"]),
                    0,
                    "now"
                )
                .await
                .unwrap(),
            ProjectCreate::Conflict
        );
        for bad in [roots(&[]), roots(&["relative"]), roots(&["/a", "/a"])] {
            assert!(store
                .create_project("p3", "request-p3", "hash", "App", &bad, 0, "now")
                .await
                .is_err());
        }
        assert!(store
            .create_project("p3", "request-p3", "hash", "  ", &roots(&["/a"]), 0, "now")
            .await
            .is_err());
    }

    // Contract: a primary edit changes future threads only; an attached thread
    // keeps its recorded cwd, and stale editors cannot overwrite a newer set.
    #[tokio::test]
    async fn root_edits_are_revisioned_and_do_not_move_conversations() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let created = project(&store, "p1", &["/work/app", "/work/docs"]).await;
        store
            .create_project_conversation(conversation("c1", "p1", Some("t1"), None))
            .await
            .unwrap();
        let updated = store
            .update_project_roots("p1", 1, &roots(&["/work/docs", "/work/app"]), 0, "later")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.roots_revision, 2);
        assert_eq!(updated.primary_root().path, "/work/docs");
        assert_eq!(
            updated
                .roots
                .iter()
                .find(|r| r.path == "/work/app")
                .unwrap()
                .id,
            created
                .roots
                .iter()
                .find(|r| r.path == "/work/app")
                .unwrap()
                .id
        );
        assert!(store
            .update_project_roots("p1", 1, &roots(&["/work/app"]), 0, "later")
            .await
            .is_err());
        assert_eq!(
            store.project_conversation("c1").await.unwrap().unwrap().cwd,
            "/work/app"
        );
        assert!(sqlx::query(
            "UPDATE project_conversations SET cwd='/work/docs' WHERE conversation_id='c1'"
        )
        .execute(&store.pool)
        .await
        .is_err());
        assert!(updated.root_for("/work/application").is_none());
    }

    // Contract: a fork records its source and cut once; the source stays unmarked.
    #[tokio::test]
    async fn a_fork_records_its_source_once() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        for (id, native) in [("source", "thread"), ("fork", "thread-fork")] {
            store
                .create_project_conversation(conversation(id, "p1", Some(native), None))
                .await
                .unwrap();
        }
        assert_eq!(store.project_fork_source("fork").await.unwrap(), None);
        store
            .record_project_fork("fork", "source", Some("turn-2"))
            .await
            .unwrap();
        assert_eq!(
            store.project_fork_source("fork").await.unwrap(),
            Some(("source".into(), Some("turn-2".into())))
        );
        assert_eq!(store.project_fork_source("source").await.unwrap(), None);
        assert!(store
            .record_project_fork("fork", "other", None)
            .await
            .is_err());
    }

    // Contract: rediscovering a native session attaches the same conversation;
    // overlapping folder groups or retries never create a duplicate.
    #[tokio::test]
    async fn native_identity_attaches_once_and_is_immutable() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        project(&store, "p2", &["/work/app", "/work/other"]).await;
        let ProjectConversationCreate::Created(first) = store
            .create_project_conversation(conversation("c1", "p1", Some("thread"), None))
            .await
            .unwrap()
        else {
            panic!()
        };
        let ProjectConversationCreate::Existing(again) = store
            .create_project_conversation(conversation("c2", "p2", Some("thread"), None))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(again.conversation_id, first.conversation_id);
        // A creation retry returns the same draft conversation.
        store
            .create_project_conversation(conversation("c3", "p1", None, Some("req")))
            .await
            .unwrap();
        let ProjectConversationCreate::Existing(retry) = store
            .create_project_conversation(conversation("c4", "p1", None, Some("req")))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(retry.conversation_id, "c3");
        store
            .set_project_native_session("c3", "native-3", "now")
            .await
            .unwrap();
        assert!(store
            .set_project_native_session("c3", "native-x", "now")
            .await
            .is_err());
        assert!(store
            .update_project_conversation(
                "c1",
                ProjectConversationPatch {
                    model: Some("claude:haiku"),
                    ..Default::default()
                },
                "now"
            )
            .await
            .is_err());
    }

    // Contract: a creation request is frozen, including its modes. A retry
    // that changes access, approval or plan mode must not adopt the first
    // conversation, and Claude-only approval cannot be stored on a Codex thread.
    #[tokio::test]
    async fn creation_retries_match_modes_and_approval_is_claude_only() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        let claude = |id, request| ProjectConversationInsert {
            family: AgentFamily::Claude,
            provider_store: "claude:home",
            claude_approval: "accept_edits",
            plan_mode: true,
            ..conversation(id, "p1", None, request)
        };
        let ProjectConversationCreate::Created(created) = store
            .create_project_conversation(claude("m1", Some("modes")))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (created.claude_approval.as_str(), created.plan_mode),
            ("accept_edits", true)
        );
        assert!(matches!(
            store.create_project_conversation(claude("m2", Some("modes"))).await.unwrap(),
            ProjectConversationCreate::Existing(c) if c.conversation_id == "m1"
        ));
        for changed in [
            ProjectConversationInsert {
                plan_mode: false,
                ..claude("m3", Some("modes"))
            },
            ProjectConversationInsert {
                claude_approval: "auto",
                ..claude("m3", Some("modes"))
            },
            ProjectConversationInsert {
                access_mode: "full_access",
                ..claude("m3", Some("modes"))
            },
            ProjectConversationInsert {
                model: Some("claude:haiku"),
                ..claude("m3", Some("modes"))
            },
            ProjectConversationInsert {
                effort: Some("high"),
                service_tier: None,
                ..claude("m3", Some("modes"))
            },
            ProjectConversationInsert {
                cwd: "/work/other",
                ..claude("m3", Some("modes"))
            },
        ] {
            assert!(store.create_project_conversation(changed).await.is_err());
        }
        assert!(store.project_conversation("m3").await.unwrap().is_none());
        for invalid in [
            ProjectConversationInsert {
                claude_approval: "yolo",
                ..claude("m4", None)
            },
            // Codex threads keep the default; the field has no meaning there.
            ProjectConversationInsert {
                family: AgentFamily::Codex,
                ..claude("m4", None)
            },
        ] {
            assert!(store.create_project_conversation(invalid).await.is_err());
        }
        store
            .create_project_conversation(conversation("codex", "p1", None, None))
            .await
            .unwrap();
        let codex_plan = store
            .update_project_conversation(
                "codex",
                ProjectConversationPatch {
                    plan_mode: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap()
            .unwrap();
        assert!(codex_plan.plan_mode);
        for bad in ["auto", "yolo"] {
            assert!(store
                .update_project_conversation(
                    "codex",
                    ProjectConversationPatch {
                        claude_approval: Some(bad),
                        ..Default::default()
                    },
                    "now",
                )
                .await
                .is_err());
        }
        // Partial updates leave the other mode alone.
        let updated = store
            .update_project_conversation(
                "m1",
                ProjectConversationPatch {
                    claude_approval: Some("auto"),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (updated.claude_approval.as_str(), updated.plan_mode),
            ("auto", true)
        );
        store
            .update_project_conversation(
                "m1",
                ProjectConversationPatch {
                    model: Some("claude:haiku"),
                    effort: Some(Some("high")),
                    access_mode: Some("full_access"),
                    plan_mode: Some(false),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        // The original request remains valid after later settings changes.
        assert!(matches!(
            store.create_project_conversation(claude("retry", Some("modes"))).await.unwrap(),
            ProjectConversationCreate::Existing(c) if c.conversation_id == "m1" && c.claude_approval == "auto"
        ));
        assert!(store
            .update_project_conversation(
                "m1",
                ProjectConversationPatch {
                    claude_approval: Some("yolo"),
                    ..Default::default()
                },
                "later",
            )
            .await
            .is_err());
        // Rows written before modes existed default to the previous behavior,
        // and the database rejects values the API would.
        sqlx::query("INSERT INTO conversations(id,codex_thread_id,session_id,created_at) VALUES('old',NULL,NULL,'now')")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,cwd,roots_revision,title,created_at,updated_at,last_activity_at) VALUES('old','p1','codex','codex:home','/work/app',1,'Old','now','now','now')")
            .execute(&store.pool).await.unwrap();
        let old = store.project_conversation("old").await.unwrap().unwrap();
        assert_eq!(
            (old.claude_approval.as_str(), old.plan_mode),
            ("ask", false)
        );
        assert!(sqlx::query(
            "UPDATE project_conversations SET claude_approval='yolo' WHERE conversation_id='old'"
        )
        .execute(&store.pool)
        .await
        .is_err());
        assert!(sqlx::query(
            "UPDATE project_conversations SET plan_mode=2 WHERE conversation_id='old'"
        )
        .execute(&store.pool)
        .await
        .is_err());
    }

    // Contract: ACP leaves permissions in OpenCode's native configuration.
    // Neutral storage can carry Plan; unsupported access cannot be persisted.
    #[tokio::test]
    async fn opencode_keeps_native_permissions_and_only_changes_plan() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        let opencode = || ProjectConversationInsert {
            family: AgentFamily::OpenCode,
            provider_store: "opencode:home",
            model: Some("opencode:local/fixture"),
            access_mode: "workspace",
            ..conversation("open", "p1", None, None)
        };
        for mode in ["read_only", "full_access"] {
            assert!(store
                .create_project_conversation(ProjectConversationInsert {
                    access_mode: mode,
                    ..opencode()
                })
                .await
                .is_err());
        }
        for approval in ["auto", "accept_edits"] {
            assert!(store
                .create_project_conversation(ProjectConversationInsert {
                    claude_approval: approval,
                    ..opencode()
                })
                .await
                .is_err());
        }
        store.create_project_conversation(opencode()).await.unwrap();
        for mode in ACCESS_MODES {
            assert!(store
                .update_project_conversation(
                    "open",
                    ProjectConversationPatch {
                        access_mode: Some(mode),
                        ..Default::default()
                    },
                    "later"
                )
                .await
                .is_err());
        }
        for approval in ["ask", "auto", "accept_edits"] {
            assert!(store
                .update_project_conversation(
                    "open",
                    ProjectConversationPatch {
                        claude_approval: Some(approval),
                        ..Default::default()
                    },
                    "later"
                )
                .await
                .is_err());
        }
        let updated = store
            .update_project_conversation(
                "open",
                ProjectConversationPatch {
                    plan_mode: Some(true),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.access_mode, "workspace");
        assert_eq!(updated.claude_approval, "ask");
        assert_eq!(updated.model.as_deref(), Some("opencode:local/fixture"));
        assert!(updated.plan_mode);
    }

    // New creation rows retain the exact client folder/request identity for
    // receipt replay. Legacy rows keep their original array payload.
    #[tokio::test]
    async fn creation_request_digest_is_frozen_and_recoverable() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        let insert = || conversation("digest-1", "p1", None, Some("digest-request"));
        assert!(matches!(
            store
                .create_project_conversation_with_request_digest(
                    insert(),
                    "folder-id-and-settings-a"
                )
                .await
                .unwrap(),
            ProjectConversationCreate::Created(_)
        ));
        let (stored_conversation, payload) = store
            .project_creation_by_request("digest-request")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_conversation.conversation_id, "digest-1");
        assert_eq!(payload["version"], 2);
        assert_eq!(payload["requestDigest"], "folder-id-and-settings-a");
        assert!(matches!(
            store.create_project_conversation_with_request_digest(insert(), "folder-id-and-settings-a").await.unwrap(),
            ProjectConversationCreate::Existing(c) if c.conversation_id == "digest-1"
        ));
        assert!(store
            .create_project_conversation_with_request_digest(insert(), "different-folder-id")
            .await
            .is_err());
        assert!(store
            .project_creation_by_request("absent")
            .await
            .unwrap()
            .is_none());

        store
            .create_project_conversation(conversation("legacy", "p1", None, Some("legacy-request")))
            .await
            .unwrap();
        let (_, legacy) = store
            .project_creation_by_request("legacy-request")
            .await
            .unwrap()
            .unwrap();
        assert!(legacy.is_array());
    }

    // Contract: omitted effort preserves it and explicit null clears it.
    // Regression: a model without effort inherits the previous model's value.
    // Owner boundary: the persistent project-conversation PATCH operation.
    #[tokio::test]
    async fn effort_patch_distinguishes_omission_value_and_clear() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        store
            .create_project_conversation(ProjectConversationInsert {
                effort: Some("high"),
                service_tier: None,
                ..conversation("c1", "p1", None, None)
            })
            .await
            .unwrap();
        for (patch, expected) in [
            (
                ProjectConversationPatch {
                    title: Some("Renamed"),
                    ..Default::default()
                },
                Some("high"),
            ),
            (
                ProjectConversationPatch {
                    effort: Some(Some("low")),
                    ..Default::default()
                },
                Some("low"),
            ),
            (
                ProjectConversationPatch {
                    effort: Some(None),
                    ..Default::default()
                },
                None,
            ),
        ] {
            let result = store
                .update_project_conversation("c1", patch, "later")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.effort.as_deref(), expected);
        }
    }

    // Contract: upgrading preserves conversations and records legacy settings
    // once, so later PATCHes cannot change the creation-retry comparison.
    // Owner boundary: the actual additive migration on the pre-upgrade table.
    #[tokio::test]
    async fn creation_payload_migration_preserves_legacy_settings() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        let original = ProjectConversationInsert {
            family: AgentFamily::Claude,
            model: Some("claude:haiku"),
            effort: Some("high"),
            service_tier: None,
            claude_approval: "auto",
            plan_mode: true,
            ..conversation("legacy", "p1", None, Some("legacy-request"))
        };
        store
            .create_project_conversation(original.clone())
            .await
            .unwrap();
        // Removing only the new column restores the exact pre-0080 shape.
        sqlx::query("ALTER TABLE project_conversations DROP COLUMN creation_payload")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/0080_project_creation_payload.sql"
        ))
        .execute(&store.pool)
        .await
        .unwrap();
        let legacy = store.project_conversation("legacy").await.unwrap().unwrap();
        assert_eq!(legacy.effort.as_deref(), Some("high"));
        assert_eq!(
            (legacy.claude_approval.as_str(), legacy.plan_mode),
            ("auto", true)
        );
        store
            .update_project_conversation(
                "legacy",
                ProjectConversationPatch {
                    effort: Some(None),
                    plan_mode: Some(false),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        assert!(matches!(
            store.create_project_conversation(original).await.unwrap(),
            ProjectConversationCreate::Existing(c) if c.conversation_id == "legacy" && c.effort.is_none() && !c.plan_mode
        ));
    }

    // A creation retry compares the selected tier, while accepted messages
    // keep their own settings after the thread is edited.
    #[tokio::test]
    async fn project_speed_retry_and_accepted_message_are_stable() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("device", "Owner", "{}", "now")
            .await
            .unwrap();
        project(&store, "p1", &["/work/app"]).await;
        let selected = ProjectConversationInsert {
            model: Some("gpt-x"),
            effort: Some("high"),
            service_tier: Some("fast"),
            ..conversation("c1", "p1", None, Some("request"))
        };
        store
            .create_project_conversation(selected.clone())
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = store
            .insert_dispatch_message("device", "client", "work", "hash", "c1", &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        store
            .update_project_conversation(
                "c1",
                ProjectConversationPatch {
                    service_tier: Some(Some("default")),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .project_conversation("c1")
                .await
                .unwrap()
                .unwrap()
                .service_tier
                .as_deref(),
            Some("default")
        );
        assert_eq!(
            store
                .project_message_execution_settings(&message.id)
                .await
                .unwrap(),
            Some((
                Some("gpt-x".into()),
                Some("high".into()),
                Some("fast".into())
            ))
        );
        assert!(matches!(
            store
                .create_project_conversation(selected.clone())
                .await
                .unwrap(),
            ProjectConversationCreate::Existing(_)
        ));
        assert!(store
            .create_project_conversation(ProjectConversationInsert {
                service_tier: Some("default"),
                ..selected
            })
            .await
            .is_err());
        // Two devices can validate against the same old settings. Once the
        // model changes, the stale speed write must lose atomically.
        store
            .update_project_conversation_if_settings_match(
                "c1",
                ProjectConversationPatch {
                    model: Some("other-model"),
                    service_tier: Some(None),
                    ..Default::default()
                },
                Some("gpt-x"),
                Some("high"),
                Some("default"),
                "next",
            )
            .await
            .unwrap();
        let stale = store
            .update_project_conversation_if_settings_match(
                "c1",
                ProjectConversationPatch {
                    service_tier: Some(Some("fast")),
                    ..Default::default()
                },
                Some("gpt-x"),
                Some("high"),
                Some("default"),
                "stale",
            )
            .await;
        assert!(matches!(
            stale,
            Err(sqlx::Error::Protocol(message)) if message == "project_settings_changed"
        ));
        let current = store.project_conversation("c1").await.unwrap().unwrap();
        assert_eq!(current.model.as_deref(), Some("other-model"));
        assert_eq!(current.service_tier, None);
    }

    // Contract: only attached, pinned conversations of included projects feed
    // the cross-project Pinned list, newest activity first and bounded.
    #[tokio::test]
    async fn pinned_conversations_follow_project_visibility_and_activity() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/a"]).await;
        project(&store, "p2", &["/work/b"]).await;
        for (id, project, activity) in [
            ("a", "p1", "2026-01-01"),
            ("b", "p2", "2026-01-03"),
            ("c", "p1", "2026-01-02"),
            ("d", "p2", "2026-01-04"),
        ] {
            store
                .create_project_conversation(conversation(id, project, None, None))
                .await
                .unwrap();
            store
                .update_project_conversation(
                    id,
                    ProjectConversationPatch {
                        pinned: Some(id != "c"),
                        ..Default::default()
                    },
                    "now",
                )
                .await
                .unwrap();
            sqlx::query(
                "UPDATE project_conversations SET last_activity_at=? WHERE conversation_id=?",
            )
            .bind(activity)
            .bind(id)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let ids = |rows: Vec<StoredProjectConversation>| {
            rows.into_iter()
                .map(|c| c.conversation_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(store.pinned_project_conversations(50).await.unwrap()),
            ["d", "b", "a"]
        );
        assert_eq!(
            ids(store.pinned_project_conversations(2).await.unwrap()),
            ["d", "b"]
        );
        store
            .update_project_metadata("p2", None, Some(false), None, "now")
            .await
            .unwrap();
        assert_eq!(
            ids(store.pinned_project_conversations(50).await.unwrap()),
            ["a"]
        );
    }

    // Contract: Project bindings stay in the Project scope; a project cannot adopt a
    // Project's runtime thread, and a project binding cannot change scope.
    #[tokio::test]
    async fn project_bindings_cannot_adopt_an_owned_session() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/app"]).await;
        bind_project_fixture(
            &store,
            "owned-conversation",
            AgentFamily::Codex,
            "owned-thread",
            None,
            "now",
        )
        .await
        .unwrap();
        store
            .create_project_conversation(conversation("c1", "p1", Some("owned-thread"), None))
            .await
            .unwrap();
        assert!(store
            .bind_project_runtime(
                "c1",
                AgentFamily::Codex,
                "codex:home",
                "owned-thread",
                None,
                "now"
            )
            .await
            .is_err());
        store
            .create_project_conversation(conversation("c2", "p1", Some("own-thread"), None))
            .await
            .unwrap();
        assert!(store
            .bind_project_runtime(
                "c2",
                AgentFamily::Claude,
                "claude:home",
                "claude-x",
                None,
                "now"
            )
            .await
            .is_err());
        store
            .bind_project_runtime(
                "c2",
                AgentFamily::Codex,
                "codex:home",
                "own-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        assert_eq!(
            store.runtime_route_for_thread("own-thread").await.unwrap(),
            Some((
                AgentFamily::Codex,
                EXECUTION_SCOPE_PROJECTS.to_owned(),
                Some("codex:home".to_owned())
            ))
        );
        assert_eq!(
            store
                .runtime_route_for_thread("owned-thread")
                .await
                .unwrap(),
            Some((
                AgentFamily::Codex,
                EXECUTION_SCOPE_PROJECTS.to_owned(),
                Some("fixture-store".to_owned())
            ))
        );
        assert!(sqlx::query(
            "UPDATE runtime_bindings SET execution_scope='invalid' WHERE conversation_id='c2'"
        )
        .execute(&store.pool)
        .await
        .is_err());
        assert_eq!(
            store
                .conversation_execution_directory("c2")
                .await
                .unwrap()
                .as_deref(),
            Some("/work/app")
        );
    }

    // Contract: the Mac computer view opens for Project chats, project threads of
    // included projects, and the owner's host-level view; nothing else.
    #[tokio::test]
    async fn computer_view_allows_project_threads_and_host_view_only() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/a"]).await;
        store
            .create_project_conversation(conversation("thread", "p1", None, None))
            .await
            .unwrap();
        for allowed in ["thread", crate::HOST_VIEW_CONVERSATION_ID] {
            assert!(
                store.computer_conversation_allowed(allowed).await.unwrap(),
                "{allowed}"
            );
        }
        for denied in ["", "unknown", "WONDER-HOST-VIEW", "wonder-host-view "] {
            assert!(
                !store.computer_conversation_allowed(denied).await.unwrap(),
                "{denied}"
            );
        }
        store
            .update_project_metadata("p1", None, Some(false), None, "now")
            .await
            .unwrap();
        assert!(!store.computer_conversation_allowed("thread").await.unwrap());
    }

    #[tokio::test]
    async fn visibility_and_pins_are_independent_metadata() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/a"]).await;
        project(&store, "p2", &["/work/b"]).await;
        store
            .update_project_metadata("p2", None, None, Some(true), "now")
            .await
            .unwrap();
        let hidden = store
            .update_project_metadata("p2", None, Some(false), None, "now")
            .await
            .unwrap()
            .unwrap();
        assert!(!hidden.is_included);
        assert_eq!(hidden.pin_order, Some(1));
        assert_eq!(store.list_projects().await.unwrap()[0].id, "p2");
        let unpinned = store
            .update_project_metadata("p2", Some("B"), Some(true), Some(false), "now")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                unpinned.name.as_str(),
                unpinned.pin_order,
                unpinned.is_included
            ),
            ("B", None, true)
        );
    }

    #[tokio::test]
    async fn project_last_model_is_kept_per_provider_and_replaced_by_the_next_choice() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        project(&store, "p1", &["/work/a"]).await;
        project(&store, "p2", &["/work/b"]).await;
        assert!(store
            .project("p1")
            .await
            .unwrap()
            .unwrap()
            .last_models
            .is_empty());
        store
            .record_project_model("p1", AgentFamily::Claude, "claude-opus", Some("high"))
            .await
            .unwrap();
        store
            .record_project_model("p1", AgentFamily::Codex, "gpt-a", None)
            .await
            .unwrap();
        // A blank model is not a choice and keeps the last one.
        store
            .record_project_model("p1", AgentFamily::Codex, " ", Some("low"))
            .await
            .unwrap();
        let last = |family, model: &str, effort: Option<&str>| ProjectLastModel {
            family,
            model: model.into(),
            effort: effort.map(Into::into),
        };
        assert_eq!(
            store
                .list_projects()
                .await
                .unwrap()
                .iter()
                .find(|p| p.id == "p1")
                .unwrap()
                .last_models,
            [
                last(AgentFamily::Claude, "claude-opus", Some("high")),
                last(AgentFamily::Codex, "gpt-a", None)
            ]
        );
        // The next choice for a provider replaces its last one, effort included.
        store
            .record_project_model("p1", AgentFamily::Claude, "claude-sonnet", None)
            .await
            .unwrap();
        assert_eq!(
            store.project("p1").await.unwrap().unwrap().last_models[0],
            last(AgentFamily::Claude, "claude-sonnet", None)
        );
        // Each Project keeps its own.
        assert!(store
            .project("p2")
            .await
            .unwrap()
            .unwrap()
            .last_models
            .is_empty());
    }

    // Contract: a Project's explicit default model (and its effort) carries
    // over as that provider's last-used model; defaults without a model,
    // and the access choice, are dropped with the table.
    #[tokio::test]
    async fn explicit_project_defaults_with_a_model_become_its_last_model() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 125)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(r#"
INSERT INTO projects(id,name,primary_root_id,created_at,updated_at) VALUES ('p','P','root','then','then'),('q','Q','root2','then','then');
INSERT INTO project_thread_defaults(project_id,agent_family,model,effort,access_mode) VALUES ('p','claude','claude-opus','high','full_access'),('q','codex',NULL,NULL,'read_only');
"#)
        .execute(&pool)
        .await
        .unwrap();
        migrations.run(&pool).await.unwrap();
        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT project_id, agent_family, model, effort FROM project_last_models",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let carried = (
            "p".into(),
            "claude".into(),
            "claude-opus".into(),
            Some("high".into()),
        );
        assert_eq!(rows, [carried]);
        let gone: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='project_thread_defaults'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gone, 0);
    }

    #[tokio::test]
    async fn auto_archive_rules_default_and_count_idle_time_from_turning_on() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        // A new or upgraded host: 3 days and on merge, counted from the upgrade.
        let rule = store.auto_archive_rule().await.unwrap();
        assert_eq!((rule.idle_days, rule.on_merge), (3, true));
        let upgraded = rule.since.clone().expect("the migration starts the clock");
        // Changing the days of a rule that is on keeps its start.
        let longer = store.set_auto_archive_rule(7, true, "t1").await.unwrap();
        assert_eq!(
            (longer.idle_days, longer.since.as_deref()),
            (7, Some(upgraded.as_str()))
        );
        // Off, then on again: the clock restarts.
        store.set_auto_archive_rule(0, false, "t2").await.unwrap();
        let again = store.set_auto_archive_rule(1, false, "t3").await.unwrap();
        assert_eq!(
            (again.idle_days, again.on_merge, again.since.as_deref()),
            (1, false, Some("t3"))
        );
        assert!(store.set_auto_archive_rule(91, true, "t4").await.is_err());
        // The worktree choice is untouched.
        assert!(store.include_worktrees().await.unwrap());

        project(&store, "p1", &["/work/a"]).await;
        let own_rule = || async { store.project("p1").await.unwrap().unwrap().auto_archive };
        assert_eq!(own_rule().await, None);
        store
            .set_project_auto_archive_rule("p1", Some((0, false)), "t5")
            .await
            .unwrap();
        store
            .set_project_auto_archive_rule("p1", Some((14, false)), "t6")
            .await
            .unwrap();
        store
            .set_project_auto_archive_rule("p1", Some((30, false)), "t7")
            .await
            .unwrap();
        assert_eq!(
            own_rule().await.unwrap(),
            AutoArchiveRule {
                idle_days: 30,
                on_merge: false,
                since: Some("t6".into())
            }
        );
        store
            .set_project_auto_archive_rule("p1", None, "t8")
            .await
            .unwrap();
        assert_eq!(own_rule().await, None);
    }

    // Contract: every title stored before titles recorded their source (a
    // rename, a fork's, a child task's, one taken from a first message) stays
    // exactly as it is: the upgrade marks each chosen and rewrites none.
    #[tokio::test]
    async fn title_source_migration_keeps_every_existing_title() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 121)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(r#"
INSERT INTO conversations(id,created_at) VALUES ('first','then'),('renamed','then'),('fork','then'),('child','then');
INSERT INTO projects(id,name,primary_root_id,last_family,created_at,updated_at) VALUES ('project','P','root','codex','then','then');
INSERT INTO project_roots(id,project_id,path,canonical_path,ordinal) VALUES ('root','project','/work','/work',0);
INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,cwd,roots_revision,title,created_at,updated_at,last_activity_at)
 VALUES ('first','project','codex','codex:home','/work',1,'GOAL','then','then','then'),
        ('renamed','project','codex','codex:home','/work',1,'My chosen name','then','then','then'),
        ('fork','project','codex','codex:home','/work',1,'GOAL (fork)','then','then','then'),
        ('child','project','codex','codex:home','/work',1,'Flaky test','then','then','then');
INSERT INTO project_conversation_forks VALUES ('fork','first',NULL);
INSERT INTO thread_delegations(child_conversation_id,parent_conversation_id,client_request_id,device_id,created_at) VALUES ('child','first','request','owner','then');
"#)
        .execute(&pool)
        .await
        .unwrap();
        migrations.run(&pool).await.unwrap();
        let chosen: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT conversation_id,title,title_chosen FROM project_conversations ORDER BY conversation_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let row = |id: &str, title: &str| (id.to_owned(), title.to_owned(), 1);
        assert_eq!(
            chosen,
            [
                row("child", "Flaky test"),
                row("first", "GOAL"),
                row("fork", "GOAL (fork)"),
                row("renamed", "My chosen name"),
            ]
        );
    }
}
