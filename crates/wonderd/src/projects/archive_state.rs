//! Whether Codex threads are archived on the Mac, read for just the threads a
//! response or action needs.
//!
//! Codex keeps each thread's archive flag in its state database
//! (`state_N.sqlite`, table `threads`). One short read-only transaction looks
//! up the requested IDs by primary key, so a list, chat open or send never pays
//! for a Project's unrelated archived history, and every read is current:
//! nothing is cached across Codex writes. When the database cannot answer (it
//! is missing, locked, replaced mid-read, has an unfinished migration, lacks
//! the columns, or has no row for a thread yet), the thread's state comes from
//! the provider's own archived list instead, and stays unknown if that fails
//! too. Unknown never means active: sends, forks and restores refuse it.
//! Lists and chat opens never wait for that provider read: they show the last
//! answer Cupola saw while it runs in the background.

use super::*;
use sqlx::{Connection, Row};
use std::path::Path;

/// A Codex thread's archive state on the Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveState {
    Archived,
    Active,
    Unknown,
}

/// Most IDs one read looks up. Lists ask only for rows they would show.
const MAX_IDS: usize = 2_000;
/// The whole database read, including opening it.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// Last answers kept for display when a later read cannot answer.
const MAX_LAST_KNOWN: usize = 4_096;

/// Each requested thread's archive flag from Codex's state database. None when
/// the database cannot answer at all; a thread missing from the map has no
/// usable row.
pub(super) async fn database_flags(home: &Path, ids: &[&str]) -> Option<HashMap<String, bool>> {
    if ids.is_empty() {
        return Some(HashMap::new());
    }
    if ids.len() > MAX_IDS || ids.iter().any(|id| id.is_empty() || id.len() > 200) {
        return None;
    }
    tokio::time::timeout(READ_TIMEOUT, read_flags(home, ids))
        .await
        .ok()
        .flatten()
}

async fn read_flags(home: &Path, ids: &[&str]) -> Option<HashMap<String, bool>> {
    if !home.is_absolute() {
        return None;
    }
    let home = crate::canonical_path::canonicalize_async(home).await.ok()?;
    let database = state_database(&home).await?;
    let before = tokio::fs::symlink_metadata(&database).await.ok()?;
    if !before.is_file() {
        return None;
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&database)
        .read_only(true)
        .busy_timeout(Duration::from_millis(25));
    let mut connection = sqlx::SqliteConnection::connect_with(&options).await.ok()?;
    let flags = query(&mut connection, ids).await;
    let _ = connection.close().await;
    let flags = flags.ok()??;
    // A database replaced while it was read describes some other store.
    let after = tokio::fs::symlink_metadata(&database).await.ok()?;
    let stamp = |metadata: &std::fs::Metadata| {
        let stamp = crate::platform::file_stamp(metadata);
        (stamp.device, stamp.inode)
    };
    (stamp(&before) == stamp(&after)).then_some(flags)
}

/// The newest `state_N.sqlite` directly in the home, as a regular file.
async fn state_database(home: &Path) -> Option<PathBuf> {
    let home = home.to_owned();
    tokio::task::spawn_blocking(move || {
        let newest = std::fs::read_dir(&home)
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let version = name
                    .to_str()?
                    .strip_prefix("state_")?
                    .strip_suffix(".sqlite")?
                    .parse::<u32>()
                    .ok()?;
                Some((version, entry.path()))
            })
            .max_by_key(|(version, _)| *version)?
            .1;
        std::fs::symlink_metadata(&newest)
            .ok()?
            .is_file()
            .then_some(newest)
    })
    .await
    .ok()
    .flatten()
}

async fn query(
    connection: &mut sqlx::SqliteConnection,
    ids: &[&str],
) -> Result<Option<HashMap<String, bool>>, sqlx::Error> {
    let mut transaction = connection.begin().await?;
    let flags = flags(&mut transaction, ids).await;
    // Never hold a read snapshot of Codex's database longer than the read.
    transaction.rollback().await?;
    flags
}

async fn flags(
    connection: &mut sqlx::SqliteConnection,
    ids: &[&str],
) -> Result<Option<HashMap<String, bool>>, sqlx::Error> {
    // sqlx records a failed or partially applied migration with success = 0.
    // A database without the table has no failed migration to report.
    let failed: Option<i64> =
        sqlx::query_scalar("SELECT COALESCE(SUM(success = 0), 0) FROM _sqlx_migrations")
            .fetch_one(&mut *connection)
            .await
            .ok();
    if failed.is_some_and(|failed| failed != 0) {
        return Ok(None);
    }
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('threads')")
        .fetch_all(&mut *connection)
        .await?;
    if !["id", "archived"]
        .iter()
        .all(|wanted| columns.iter().any(|column| column == wanted))
    {
        return Ok(None);
    }
    // One statement, looked up by primary key: SQLite probes the index once
    // per requested ID.
    let rows = sqlx::query(
        "SELECT id, archived FROM threads WHERE id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(ids).unwrap_or_default())
    .fetch_all(&mut *connection)
    .await?;
    let mut flags = HashMap::with_capacity(rows.len());
    for row in rows {
        // A value of an unexpected type leaves that thread unknown.
        let (Ok(id), Ok(archived)) = (row.try_get::<String, _>(0), row.try_get::<i64, _>(1)) else {
            continue;
        };
        flags.insert(id, archived != 0);
    }
    Ok(Some(flags))
}

/// Each Codex conversation's archive state, keyed by native thread ID, fresh
/// enough to admit an action: threads the database cannot answer wait for the
/// provider's archived list of their folders. Other families and drafts are
/// left out.
pub(crate) async fn archive_states(
    state: &AppState,
    conversations: &[StoredProjectConversation],
) -> HashMap<String, ArchiveState> {
    let (mut states, unanswered) = database_states(state, conversations).await;
    if !unanswered.is_empty() {
        let found = provider_states(state, &unanswered).await;
        remember(state, &found);
        states.extend(found);
    }
    states
}

/// For lists and chat opens: what the database answers now. A thread it
/// cannot answer is Unknown here (callers show the last answer through
/// `displayed`) and is checked against the provider in the background, so no
/// response waits for a Project's archived history. Never an admission
/// decision.
pub(crate) async fn display_states(
    state: &AppState,
    conversations: &[StoredProjectConversation],
) -> HashMap<String, ArchiveState> {
    let (mut states, unanswered) = database_states(state, conversations).await;
    for conversation in &unanswered {
        if let Some(native) = &conversation.native_session_id {
            states.insert(native.clone(), ArchiveState::Unknown);
        }
    }
    refresh_in_background(state, unanswered);
    states
}

/// The database's answers, plus the default-store threads it could not
/// answer. Answers are remembered for `displayed`.
async fn database_states(
    state: &AppState,
    conversations: &[StoredProjectConversation],
) -> (
    HashMap<String, ArchiveState>,
    Vec<StoredProjectConversation>,
) {
    let mut states = HashMap::new();
    let mut default_store = Vec::new();
    for conversation in conversations
        .iter()
        .filter(|c| c.family == AgentFamily::Codex)
    {
        let Some(native) = conversation.native_session_id.as_deref() else {
            continue;
        };
        if conversation.provider_store == state.projects.codex_store {
            default_store.push(conversation);
            continue;
        }
        // This native history cannot be checked; display uses Cupola metadata.
        states.insert(native.to_owned(), ArchiveState::Unknown);
    }
    let ids = default_store
        .iter()
        .filter_map(|c| c.native_session_id.as_deref())
        .collect::<Vec<_>>();
    let flags = database_flags(&state.codex_home, &ids)
        .await
        .unwrap_or_default();
    let mut answered = HashMap::new();
    let mut unanswered = Vec::new();
    for conversation in default_store {
        let native = conversation.native_session_id.clone().unwrap_or_default();
        match flags.get(&native) {
            Some(true) => answered.insert(native, ArchiveState::Archived),
            Some(false) => answered.insert(native, ArchiveState::Active),
            None => {
                unanswered.push(conversation.clone());
                None
            }
        };
    }
    remember(state, &answered);
    states.extend(answered);
    (states, unanswered)
}

/// The provider's archived list, read for just these threads' folders.
async fn provider_states(
    state: &AppState,
    unanswered: &[StoredProjectConversation],
) -> HashMap<String, ArchiveState> {
    let archived = archived_codex_ids(state, unanswered).await;
    unanswered
        .iter()
        .filter_map(|c| c.native_session_id.clone())
        .map(|native| {
            let found = match &archived {
                Ok(ids) if ids.contains(&native) => ArchiveState::Archived,
                Ok(_) => ArchiveState::Active,
                Err(_) => ArchiveState::Unknown,
            };
            (native, found)
        })
        .collect()
}

/// Reads the provider's archived list for these threads off the request
/// path, one read per folder selection at a time. When it changes what a
/// list would show, phones read their Chats again.
fn refresh_in_background(state: &AppState, unanswered: Vec<StoredProjectConversation>) {
    if unanswered.is_empty() {
        return;
    }
    let mut key = unanswered.iter().map(|c| c.cwd.clone()).collect::<Vec<_>>();
    key.sort();
    key.dedup();
    if !state
        .projects
        .archive_refreshes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone())
    {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        // `archived_codex_ids` bounds the read; Unknown is not remembered.
        let found = provider_states(&state, &unanswered).await;
        let changed = remember(&state, &found);
        state
            .projects
            .archive_refreshes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if changed {
            announce_chats_changed(&state).await;
        }
    });
}

/// One conversation's archive state; Active for non-Codex threads and drafts.
pub(crate) async fn archive_state(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> ArchiveState {
    let Some(native) = conversation
        .native_session_id
        .as_deref()
        .filter(|_| conversation.family == AgentFamily::Codex)
    else {
        return ArchiveState::Active;
    };
    archive_states(state, std::slice::from_ref(conversation))
        .await
        .get(native)
        .copied()
        .unwrap_or(ArchiveState::Unknown)
}

/// One conversation's state for a chat open; see `display_states`.
pub(crate) async fn display_state(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> ArchiveState {
    let Some(native) = conversation
        .native_session_id
        .as_deref()
        .filter(|_| conversation.family == AgentFamily::Codex)
    else {
        return ArchiveState::Active;
    };
    display_states(state, std::slice::from_ref(conversation))
        .await
        .get(native)
        .copied()
        .unwrap_or(ArchiveState::Unknown)
}

/// For display only: the state, or Cupola's last answer when it is unknown
/// now. Never an admission decision.
pub(crate) fn displayed(
    state: &AppState,
    conversation: &StoredProjectConversation,
    current: ArchiveState,
) -> ArchiveState {
    // Retained legacy history is visible using Cupola's archive choice. It
    // has no runnable provider home; mutation admission checks store identity.
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return if conversation.wonder_archived {
            ArchiveState::Archived
        } else {
            ArchiveState::Active
        };
    }
    if current != ArchiveState::Unknown {
        return current;
    }
    conversation
        .native_session_id
        .as_ref()
        .and_then(|native| {
            state
                .projects
                .archive_known
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(conversation.provider_store.clone(), native.clone()))
                .copied()
        })
        .map_or(ArchiveState::Unknown, |archived| {
            if archived {
                ArchiveState::Archived
            } else {
                ArchiveState::Active
            }
        })
}

/// Keeps the default store's answers for `displayed`; true when one differs
/// from the last answer.
fn remember(state: &AppState, states: &HashMap<String, ArchiveState>) -> bool {
    let store = &state.projects.codex_store;
    let mut known = state
        .projects
        .archive_known
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for (native, found) in states {
        let archived = match found {
            ArchiveState::Archived => true,
            ArchiveState::Active => false,
            ArchiveState::Unknown => continue,
        };
        if known.len() >= MAX_LAST_KNOWN {
            known.clear();
        }
        changed |= known.insert((store.clone(), native.clone()), archived) != Some(archived);
    }
    changed
}

/// Cupola just changed a thread's archive in Codex, so the next display
/// fallback starts from the new state.
pub(super) fn record(state: &AppState, native: &str, archived: bool) {
    let mut known = state
        .projects
        .archive_known
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if known.len() >= MAX_LAST_KNOWN {
        known.clear();
    }
    known.insert(
        (state.projects.codex_store.clone(), native.to_owned()),
        archived,
    );
}
