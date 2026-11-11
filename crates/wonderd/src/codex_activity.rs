//! Read-only lifecycle evidence from Codex's own indexed history. The App
//! Server serving Wonder reinterprets another process's unfinished turn as
//! interrupted. Its native index retains the producer's actual lifecycle.
//!
//! This is a private on-disk compatibility adapter for Codex's
//! thread_history_1 index. Codex migrates it forward on its own schedule
//! (ChatGPT's bundled alpha builds and the released CLI differ), so the reader
//! checks for the columns it queries, not for a migration count: lifecycle
//! needs the turn and projection columns, and the final reply is read only
//! when the item columns exist too. A partially applied migration, a missing
//! column or a missing index is unknown evidence, never idle and never a
//! provider failure.
//!
//! Every caller (the push watcher, list rows, dispatch admission) reads
//! through one owner per Codex home, which reuses its read-only connections
//! and caches the column check until the index is replaced, changes shape or
//! records a migration. Only the index's own answer is reused, never an
//! activity: each read still checks the rollout, the index identity, the
//! quiet threshold and the writer.
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sqlx::{Connection, Row};

const DEADLINE: Duration = Duration::from_millis(250);
const MAX_IDLE: Duration = Duration::from_secs(15 * 60);
const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// Concurrent reads across all homes, and so connections in use at once.
const MAX_READERS: usize = 4;
/// Connections a home keeps between reads, and how long an unused one stays.
const MAX_SPARE_CONNECTIONS: usize = 2;
const CONNECTION_IDLE: Duration = Duration::from_secs(60);
/// Codex homes with a reader; production has one. Tests each use their own
/// home, in parallel, so they keep more rather than evict each other's.
const MAX_HOMES: usize = if cfg!(test) { 256 } else { 4 };
static READERS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_READERS)));

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activity {
    pub turn: String,
    pub status: &'static str,
    pub reply: Option<String>,
}

impl Activity {
    pub(crate) fn working(&self) -> bool {
        self.status == "inProgress"
    }
}

enum Lifecycle {
    Empty,
    Turn(Activity),
}

pub(crate) async fn read(home: &Path, thread: &str, path: &Path) -> Option<Activity> {
    match lifecycle(home, thread, path).await? {
        Lifecycle::Empty => None,
        Lifecycle::Turn(activity) => Some(activity),
    }
}

/// Admission also recognizes a fully indexed, metadata-only thread. Absence
/// of a turn row by itself is never idle evidence.
pub(crate) async fn busy(home: &Path, thread: &str, path: &Path) -> Option<bool> {
    lifecycle(home, thread, path)
        .await
        .map(|evidence| match evidence {
            Lifecycle::Empty => false,
            Lifecycle::Turn(activity) => activity.working(),
        })
}

async fn lifecycle(home: &Path, thread: &str, path: &Path) -> Option<Lifecycle> {
    tokio::time::timeout(DEADLINE, async {
        let permit = READERS.clone().acquire_owned().await.ok()?;
        let (home, thread, path) = (home.to_owned(), thread.to_owned(), path.to_owned());
        let runtime = tokio::runtime::Handle::current();
        // A cancelled caller cannot cancel an OS file read. Its permit stays
        // with that operation until it actually exits, so repeated timeouts
        // cannot accumulate unbounded background readers.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            runtime.block_on(lifecycle_at(
                &home,
                &thread,
                &path,
                SystemTime::now(),
                live_writer_started,
            ))
        })
        .await
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten()
}

/// Whether the thread's latest turn is running, for list rows. A rollout
/// unwritten for MAX_IDLE with no live writer cannot hold a running turn, so
/// it is answered from one stat without opening Codex's index.
pub(crate) async fn working(home: &Path, thread: &str, path: &Path) -> bool {
    let probe = path.to_owned();
    let quiet = tokio::time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || {
            let meta = std::fs::symlink_metadata(&probe).ok()?;
            let quiet = SystemTime::now()
                .duration_since(meta.modified().ok()?)
                .unwrap_or_default();
            Some(quiet > MAX_IDLE && live_writer_started(&meta).is_none())
        }),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();
    match quiet {
        None | Some(true) => false,
        Some(false) => read(home, thread, path)
            .await
            .is_some_and(|activity| activity.working()),
    }
}

async fn lifecycle_at(
    home: &Path,
    thread: &str,
    path: &Path,
    now: SystemTime,
    writer_started: fn(&std::fs::Metadata) -> Option<SystemTime>,
) -> Option<Lifecycle> {
    if thread.is_empty() || thread.len() > 200 || !home.is_absolute() || !path.is_absolute() {
        return None;
    }
    let home = crate::canonical_path::canonicalize_async(home).await.ok()?;
    let canonical = crate::canonical_path::canonicalize_async(path).await.ok()?;
    if canonical != path
        || !(path.starts_with(home.join("sessions"))
            || path.starts_with(home.join("archived_sessions")))
        || !path
            .file_name()?
            .to_str()?
            .ends_with(&format!("-{thread}.jsonl"))
    {
        return None;
    }
    let before = tokio::fs::symlink_metadata(path).await.ok()?;
    if !before.is_file() {
        return None;
    }
    let database = home.join("thread_history_1.sqlite");
    let database_before = tokio::fs::symlink_metadata(&database).await.ok()?;
    if !database_before.is_file() {
        return None;
    }
    let reader = Reader::of(&home);
    let indexed = reader
        .indexed(&database, identity(&database_before), thread)
        .await?;
    let row = match indexed {
        Indexed::Turn(row) => row,
        Indexed::Empty(indexed) => {
            return empty(path, thread, indexed, &before, &database, &database_before).await;
        }
    };
    let TurnRow {
        turn,
        status,
        start,
        end,
        completed,
        indexed,
        reply,
    } = row;
    if turn.is_empty()
        || turn.len() > 200
        || start < 0
        || indexed <= start
        || indexed as u64 > before.len()
    {
        return None;
    }
    let started = start_record(path, start as u64, &turn)?;
    let status = match status.as_str() {
        "inProgress" if end.is_none() && completed.is_none() => {
            let quiet = now
                .duration_since(before.modified().ok()?)
                .unwrap_or_default();
            // A long tool call can leave the rollout unwritten for a while,
            // but an abandoned turn also stays inProgress in the index. Past
            // MAX_IDLE the turn counts as running only while Codex's index has
            // read the whole rollout and the process writing it is alive and
            // started before the turn; otherwise it is unknown.
            if quiet > MAX_IDLE
                && (indexed as u64 != before.len()
                    || !started
                        .zip(writer_started(&before))
                        .is_some_and(|(turn, writer)| writer <= turn))
            {
                return None;
            }
            "inProgress"
        }
        // A lagging projection cannot prove idle: the unread suffix may
        // already contain another turn. No fixed suffix window can prove it.
        "completed" | "interrupted" | "failed"
            if indexed as u64 == before.len()
                && end.is_some_and(|end| {
                    end >= start
                        && end < indexed
                        && (completed.is_some()
                            || terminal_record(path, end as u64, indexed as u64, &turn, &status))
                }) =>
        {
            match status.as_str() {
                "completed" => "completed",
                "interrupted" => "interrupted",
                _ => "failed",
            }
        }
        _ => return None,
    };
    let after = tokio::fs::symlink_metadata(path).await.ok()?;
    let database_after = tokio::fs::symlink_metadata(database).await.ok()?;
    if !same_file(&before, &after) || !same_identity(&database_before, &database_after) {
        return None;
    }
    let reply = reply
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|item| item["type"] == "agentMessage")
        .and_then(|item| {
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    Some(Lifecycle::Turn(Activity {
        turn,
        status,
        reply,
    }))
}

/// A thread the index holds no turn for is empty only when its rollout is
/// exactly its own metadata record and the projection has read all of it.
async fn empty(
    path: &Path,
    thread: &str,
    indexed: i64,
    before: &std::fs::Metadata,
    database: &Path,
    database_before: &std::fs::Metadata,
) -> Option<Lifecycle> {
    let (meta, meta_bytes) = record(path, 0)?;
    if meta["type"] != "session_meta" || meta["payload"]["id"] != thread {
        return None;
    }
    // A missing projection, an unread suffix, or any record after metadata
    // needs real turn evidence. Never infer empty from missing rows alone.
    (indexed >= 0
        && indexed as u64 == before.len()
        && meta_bytes == before.len()
        && same_file(before, &tokio::fs::symlink_metadata(path).await.ok()?)
        && same_identity(
            database_before,
            &tokio::fs::symlink_metadata(database).await.ok()?,
        ))
    .then_some(Lifecycle::Empty)
}

/// The index's answer for one thread, read in one short transaction.
enum Indexed {
    Turn(TurnRow),
    /// No turn row; how far the projection has read the rollout.
    Empty(i64),
}

/// The thread's latest turn row and its projection's progress.
struct TurnRow {
    turn: String,
    status: String,
    start: i64,
    end: Option<i64>,
    completed: Option<i64>,
    indexed: i64,
    reply: Option<String>,
}

/// The index file's (device, inode).
type Identity = (u64, u64);

fn identity(metadata: &std::fs::Metadata) -> Identity {
    let stamp = crate::platform::file_stamp(metadata);
    (stamp.device, stamp.inode)
}

/// What decides the cached column check: the index file, its schema cookie
/// (bumped by every table or column change) and its recorded migrations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SchemaKey {
    identity: Identity,
    version: i64,
    /// (count, failed, latest version); None without a migrations table.
    migrations: Option<(i64, i64, i64)>,
}

/// The read owner for one Codex home. Reads reuse its connections, and the
/// column check runs once per index identity, schema version and migration
/// record. A replaced index drops both.
struct Reader {
    state: Mutex<ReaderState>,
}

#[derive(Default)]
struct ReaderState {
    /// Unused connections to the index identity they were opened for, with
    /// when each was last returned.
    spare: Vec<(Identity, tokio::time::Instant, sqlx::SqliteConnection)>,
    schema: Option<(SchemaKey, Option<Schema>)>,
    reaping: bool,
    #[cfg(test)]
    opened: usize,
    #[cfg(test)]
    schema_checks: usize,
}

/// Each home's reader, least recently used first.
type Homes = Vec<(PathBuf, Arc<Reader>)>;
static HOMES: LazyLock<Mutex<Homes>> = LazyLock::new(Mutex::default);

impl Reader {
    /// The canonical home's reader, most recently used last.
    fn of(home: &Path) -> Arc<Self> {
        let mut homes = HOMES.lock().unwrap_or_else(|e| e.into_inner());
        let reader = match homes.iter().position(|(path, _)| path == home) {
            Some(at) => homes.remove(at).1,
            None => Arc::new(Self {
                state: Mutex::default(),
            }),
        };
        homes.push((home.to_owned(), reader.clone()));
        if homes.len() > MAX_HOMES {
            homes.remove(0);
        }
        reader
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ReaderState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// None when the index cannot answer: it is locked, corrupt, missing a
    /// column this reader queries or has an unfinished migration.
    async fn indexed(
        self: &Arc<Self>,
        database: &Path,
        identity: Identity,
        thread: &str,
    ) -> Option<Indexed> {
        let spare = {
            let mut state = self.state();
            // A replaced index makes every connection and the column check
            // stale: they describe the file that was there before.
            if state.spare.iter().any(|(opened, _, _)| *opened != identity) {
                state.spare.clear();
            }
            if state
                .schema
                .is_some_and(|(key, _)| key.identity != identity)
            {
                state.schema = None;
            }
            state.spare.pop()
        };
        let mut connection = match spare {
            Some((_, _, connection)) => connection,
            None => {
                let options = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(database)
                    .read_only(true)
                    .busy_timeout(Duration::from_millis(25));
                let connection = sqlx::SqliteConnection::connect_with(&options).await.ok()?;
                #[cfg(test)]
                {
                    self.state().opened += 1;
                }
                connection
            }
        };
        // Any error discards the connection rather than reuse one in an
        // unknown state; the next read opens a fresh one.
        let indexed = self.query(&mut connection, identity, thread).await.ok()?;
        self.put(identity, connection);
        indexed
    }

    async fn query(
        &self,
        connection: &mut sqlx::SqliteConnection,
        identity: Identity,
        thread: &str,
    ) -> Result<Option<Indexed>, sqlx::Error> {
        let mut transaction = connection.begin().await?;
        let indexed = self.latest(&mut transaction, identity, thread).await;
        // A kept connection must not pin a read snapshot of Codex's index.
        transaction.rollback().await?;
        indexed
    }

    async fn latest(
        &self,
        connection: &mut sqlx::SqliteConnection,
        identity: Identity,
        thread: &str,
    ) -> Result<Option<Indexed>, sqlx::Error> {
        let Some(schema) = self.schema(connection, identity).await? else {
            return Ok(None);
        };
        // The existing (thread_id, rollout_ordinal) index makes this independent
        // of transcript size. Keep reply allocation bounded inside SQLite too.
        macro_rules! latest_turn {
            ($reply:literal, $join:literal) => {
                concat!(
                    "SELECT substr(t.turn_id,1,201) AS turn_id, substr(t.status,1,32) AS status, t.completed_at, t.rollout_byte_offset, \
                     t.rollout_end_byte_offset, p.next_rollout_byte_offset, ",
                    $reply,
                    " FROM thread_turns t JOIN thread_history_projection_state p USING(thread_id) ",
                    $join,
                    " WHERE t.thread_id=? ORDER BY t.rollout_ordinal DESC LIMIT 1"
                )
            };
        }
        let query = match schema {
            Schema::Lifecycle => latest_turn!("NULL AS reply_json", ""),
            Schema::Replies => latest_turn!(
                "substr(i.item_json, 1, 65536) AS reply_json",
                "LEFT JOIN thread_items i ON i.thread_id=t.thread_id AND i.turn_id=t.turn_id \
                 AND i.item_id=t.final_agent_item_id"
            ),
        };
        let row = sqlx::query(query)
            .bind(thread)
            .fetch_optional(&mut *connection)
            .await?;
        let Some(row) = row else {
            let indexed: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",
            )
            .bind(thread)
            .fetch_optional(&mut *connection)
            .await?;
            return Ok(indexed.flatten().map(Indexed::Empty));
        };
        // A value of an unexpected type is unknown evidence, not a broken
        // connection.
        let turn = (|| {
            Some(TurnRow {
                turn: row.try_get("turn_id").ok()?,
                status: row.try_get("status").ok()?,
                start: row.try_get("rollout_byte_offset").ok()?,
                end: row.try_get("rollout_end_byte_offset").ok()?,
                completed: row.try_get("completed_at").ok()?,
                indexed: row.try_get("next_rollout_byte_offset").ok()?,
                reply: row
                    .try_get::<Option<String>, _>("reply_json")
                    .ok()
                    .flatten(),
            })
        })();
        Ok(turn.map(Indexed::Turn))
    }

    /// The cached column check, rerun when the index's identity, schema
    /// version or migration record differs from the one it was made for.
    async fn schema(
        &self,
        connection: &mut sqlx::SqliteConnection,
        identity: Identity,
    ) -> Result<Option<Schema>, sqlx::Error> {
        let version: i64 = sqlx::query_scalar("PRAGMA schema_version")
            .fetch_one(&mut *connection)
            .await?;
        // A database without the table at all has no failed migration to report.
        let migrations = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT COUNT(*), COALESCE(SUM(success = 0), 0), COALESCE(MAX(version), 0) \
             FROM _sqlx_migrations",
        )
        .fetch_one(&mut *connection)
        .await
        .ok();
        let key = SchemaKey {
            identity,
            version,
            migrations,
        };
        let cached = self.state().schema;
        if let Some((cached, schema)) = cached {
            if cached == key {
                return Ok(schema);
            }
        }
        let schema = schema(connection, migrations).await?;
        let mut state = self.state();
        state.schema = Some((key, schema));
        #[cfg(test)]
        {
            state.schema_checks += 1;
        }
        Ok(schema)
    }

    /// Keeps a connection for the next read, closing unused ones later.
    fn put(self: &Arc<Self>, identity: Identity, connection: sqlx::SqliteConnection) {
        // Without a runtime to close it later, a connection is not kept.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut state = self.state();
        if state.spare.len() >= MAX_SPARE_CONNECTIONS
            || state.spare.iter().any(|(opened, _, _)| *opened != identity)
        {
            return;
        }
        state
            .spare
            .push((identity, tokio::time::Instant::now(), connection));
        if !std::mem::replace(&mut state.reaping, true) {
            runtime.spawn(self.clone().reap());
        }
    }

    /// Closes connections left unused for CONNECTION_IDLE, and ends once
    /// none remain.
    async fn reap(self: Arc<Self>) {
        loop {
            tokio::time::sleep(CONNECTION_IDLE).await;
            let (idle, done): (Vec<_>, bool) = {
                let mut state = self.state();
                let (idle, kept) = std::mem::take(&mut state.spare)
                    .into_iter()
                    .partition(|(_, used, _)| used.elapsed() >= CONNECTION_IDLE);
                state.spare = kept;
                state.reaping = !state.spare.is_empty();
                (idle, !state.reaping)
            };
            for (_, _, connection) in idle {
                let _ = connection.close().await;
            }
            if done {
                return;
            }
        }
    }
}

/// What the installed index supports, from its columns rather than its
/// migration numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Schema {
    /// Turn lifecycle only: the final reply text is not read.
    Lifecycle,
    /// Lifecycle and the final agent reply.
    Replies,
}

const LIFECYCLE_COLUMNS: &[(&str, &[&str])] = &[
    (
        "thread_turns",
        &[
            "thread_id",
            "turn_id",
            "rollout_ordinal",
            "status",
            "completed_at",
            "rollout_byte_offset",
            "rollout_end_byte_offset",
        ],
    ),
    (
        "thread_history_projection_state",
        &["thread_id", "next_rollout_byte_offset"],
    ),
];
const REPLY_COLUMNS: &[(&str, &[&str])] = &[
    ("thread_turns", &["final_agent_item_id"]),
    (
        "thread_items",
        &["thread_id", "turn_id", "item_id", "item_json"],
    ),
];

/// None when the index cannot answer lifecycle questions: a migration that
/// did not finish, or a column this reader queries is missing.
async fn schema(
    connection: &mut sqlx::SqliteConnection,
    migrations: Option<(i64, i64, i64)>,
) -> Result<Option<Schema>, sqlx::Error> {
    // sqlx records a failed or partially applied migration with success = 0.
    if migrations.is_some_and(|(_, failed, _)| failed != 0) {
        return Ok(None);
    }
    async fn has(
        connection: &mut sqlx::SqliteConnection,
        wanted: &[(&str, &[&str])],
    ) -> Result<bool, sqlx::Error> {
        for (table, columns) in wanted {
            let present: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&mut *connection)
                .await?;
            if !columns.iter().all(|c| present.iter().any(|p| p == c)) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    if !has(connection, LIFECYCLE_COLUMNS).await? {
        return Ok(None);
    }
    Ok(Some(if has(connection, REPLY_COLUMNS).await? {
        Schema::Replies
    } else {
        Schema::Lifecycle
    }))
}

#[cfg(all(test, unix))]
async fn read_at(
    home: &Path,
    thread: &str,
    path: &Path,
    now: SystemTime,
    writer_started: fn(&std::fs::Metadata) -> Option<SystemTime>,
) -> Option<Activity> {
    match lifecycle_at(home, thread, path, now, writer_started).await? {
        Lifecycle::Empty => None,
        Lifecycle::Turn(activity) => Some(activity),
    }
}

/// Native history may omit completed_at even for a timestamped terminal
/// rollout event. In that case require the exact indexed terminal record,
/// its turn identity and a complete projection, rather than guessing idle.
fn terminal_record(path: &Path, offset: u64, indexed: u64, turn: &str, status: &str) -> bool {
    let Some((value, bytes)) = record(path, offset) else {
        return false;
    };
    if offset.checked_add(bytes).is_none_or(|end| end > indexed)
        || value["type"] != "event_msg"
        || value["payload"]["turn_id"] != turn
    {
        return false;
    }
    match status {
        "completed" => value["payload"]["type"] == "task_complete",
        "interrupted" => {
            value["payload"]["type"] == "turn_aborted"
                && value["payload"]["reason"] == "interrupted"
        }
        "failed" => value["payload"]["type"] == "error",
        _ => false,
    }
}

/// The turn's `task_started` record at `offset`, or None when the index's
/// offset does not point at this turn's start. Its value is when Codex wrote
/// it (the record's `timestamp`), if present.
fn start_record(path: &Path, offset: u64, turn: &str) -> Option<Option<SystemTime>> {
    let (value, _) = record(path, offset)?;
    if value["type"] != "event_msg"
        || value["payload"]["type"] != "task_started"
        || value["payload"]["turn_id"] != turn
    {
        return None;
    }
    Some(
        value["timestamp"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(SystemTime::from),
    )
}

fn record(path: &Path, offset: u64) -> Option<(serde_json::Value, u64)> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut line = Vec::new();
    BufReader::new(file.take(MAX_RECORD_BYTES))
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.last() != Some(&b'\n') {
        return None;
    }
    Some((serde_json::from_slice(&line).ok()?, line.len() as u64))
}

/// When the earliest live process that has `file` open for writing started.
/// Codex keeps a loaded thread's rollout open for appending (measured with
/// lsof on the pinned desktop app), so this is the producer's own liveness.
/// None when no such process was found or the scan could not finish.
pub(crate) fn live_writer_started(file: &std::fs::Metadata) -> Option<SystemTime> {
    let stamp = crate::platform::file_stamp(file);
    writers::started(stamp.device, stamp.inode)
}

/// Regular files this user's live processes hold open for writing, with the
/// earliest holder's start time.
type Writers = std::collections::HashMap<(u64, u64), SystemTime>;

/// One process-table scan answers every lookup in its window, whether or not
/// it finished, so a list of quiet rows never scans once per row.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Scans {
    fresh: Duration,
    last: Mutex<Option<(Instant, Option<Arc<Writers>>)>>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Scans {
    const fn new(fresh: Duration) -> Self {
        Self {
            fresh,
            last: Mutex::new(None),
        }
    }

    /// None when the window's scan could not finish.
    fn writers(&self, scan: impl FnOnce() -> Option<Writers>) -> Option<Arc<Writers>> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match last.as_ref() {
            Some((at, writers)) if at.elapsed() < self.fresh => writers.clone(),
            _ => {
                let writers = scan().map(Arc::new);
                *last = Some((Instant::now(), writers.clone()));
                writers
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod writers {
    use std::time::{Duration, Instant, SystemTime};

    use super::{Scans, Writers};

    const SCAN_DEADLINE: Duration = Duration::from_millis(150);
    const MAX_PIDS: usize = 8192;
    const MAX_FDS: usize = 4096;
    /// `<sys/proc_info.h>`: `PROC_PIDFDVNODEINFO` and `FWRITE`; `VREG` from `<sys/vnode.h>`.
    const PROC_PIDFDVNODEINFO: i32 = 1;
    const FWRITE: u32 = 0x2;
    const VREG: i32 = 1;

    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfo {
        pfi: ProcFileInfo,
        pvi: libc::vnode_info,
    }

    static SCANS: Scans = Scans::new(Duration::from_secs(1));

    pub(super) fn started(dev: u64, ino: u64) -> Option<SystemTime> {
        SCANS.writers(scan)?.get(&(dev, ino)).copied()
    }

    fn scan() -> Option<Writers> {
        let deadline = Instant::now() + SCAN_DEADLINE;
        let uid = unsafe { libc::getuid() };
        let mut pids = vec![0i32; MAX_PIDS];
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if count <= 0 || count as usize >= MAX_PIDS {
            return None;
        }
        pids.truncate(count as usize);
        let mut writers = Writers::new();
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            MAX_FDS
        ];
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            if Instant::now() >= deadline {
                return None;
            }
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            // Another user's process (or one that just exited) is not this
            // owner's Codex.
            if read != size || info.pbi_uid != uid {
                continue;
            }
            let start = SystemTime::UNIX_EPOCH
                + Duration::from_secs(info.pbi_start_tvsec)
                + Duration::from_micros(info.pbi_start_tvusec);
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast(),
                    (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as i32,
                )
            };
            if bytes <= 0 {
                continue;
            }
            let listed = bytes as usize / std::mem::size_of::<libc::proc_fdinfo>();
            for fd in &fds[..listed.min(MAX_FDS)] {
                if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                    continue;
                }
                let mut vnode: VnodeFdInfo = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<VnodeFdInfo>() as i32;
                let read = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDVNODEINFO,
                        (&mut vnode as *mut VnodeFdInfo).cast(),
                        size,
                    )
                };
                if read != size || vnode.pfi.fi_openflags & FWRITE == 0 || vnode.pvi.vi_type != VREG
                {
                    continue;
                }
                let key = (
                    u64::from(vnode.pvi.vi_stat.vst_dev),
                    vnode.pvi.vi_stat.vst_ino,
                );
                writers
                    .entry(key)
                    .and_modify(|earliest| *earliest = (*earliest).min(start))
                    .or_insert(start);
            }
        }
        Some(writers)
    }
}

#[cfg(not(target_os = "macos"))]
mod writers {
    pub(super) fn started(_dev: u64, _ino: u64) -> Option<std::time::SystemTime> {
        None
    }
}

fn same_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    let (left, right) = (
        crate::platform::file_stamp(left),
        crate::platform::file_stamp(right),
    );
    left.device == right.device && left.inode == right.inode
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    same_identity(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use sqlx::Executor;

    pub(crate) struct Fixture {
        pub(crate) dir: tempfile::TempDir,
        pub(crate) path: std::path::PathBuf,
        db: sqlx::SqliteConnection,
        start: u64,
    }

    impl Fixture {
        async fn new() -> Self {
            Self::new_for_thread("thread").await
        }
        pub(crate) async fn new_for_thread(thread: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = crate::canonical_path::canonicalize(dir.path()).unwrap();
            let parent = home.join("sessions/2026/10/13");
            std::fs::create_dir_all(&parent).unwrap();
            let path = parent.join(format!("rollout-2026-10-13T00-00-00-{thread}.jsonl"));
            let prefix =
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread}\"}}}}\n");
            std::fs::write(&path, format!("{prefix}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn\"}}}}\n")).unwrap();
            let mut db = sqlx::SqliteConnection::connect_with(
                &sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(home.join("thread_history_1.sqlite"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
            db.execute("CREATE TABLE _sqlx_migrations(version INTEGER, success INTEGER);
                INSERT INTO _sqlx_migrations VALUES(1,1),(2,1),(3,1),(4,1),(5,1),(6,1),(7,1),(8,1);
                CREATE TABLE thread_turns(thread_id TEXT, turn_id TEXT, rollout_ordinal INTEGER, status TEXT,
                    completed_at INTEGER, rollout_byte_offset INTEGER, rollout_end_byte_offset INTEGER, final_agent_item_id TEXT,
                    PRIMARY KEY(thread_id,turn_id));
                CREATE UNIQUE INDEX idx_thread_turns_page ON thread_turns(thread_id,rollout_ordinal);
                CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);
                CREATE TABLE thread_items(thread_id TEXT,turn_id TEXT,item_id TEXT,item_json TEXT,PRIMARY KEY(thread_id,turn_id,item_id));").await.unwrap();
            sqlx::query(
                "INSERT INTO thread_turns VALUES(?,'turn',1,'inProgress',NULL,?,NULL,NULL)",
            )
            .bind(thread)
            .bind(prefix.len() as i64)
            .execute(&mut db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO thread_history_projection_state VALUES(?,?)")
                .bind(thread)
                .bind(std::fs::metadata(&path).unwrap().len() as i64)
                .execute(&mut db)
                .await
                .unwrap();
            Self {
                dir,
                path,
                db,
                start: prefix.len() as u64,
            }
        }
        async fn read(&self) -> Option<Activity> {
            read(self.dir.path(), "thread", &self.path).await
        }
        /// Connections this home's reader opened and column checks it ran.
        fn reader(&self) -> (usize, usize) {
            let reader = Reader::of(&crate::canonical_path::canonicalize(self.dir.path()).unwrap());
            let state = reader.state();
            (state.opened, state.schema_checks)
        }
        async fn indexed(&mut self) {
            sqlx::query("UPDATE thread_history_projection_state SET next_rollout_byte_offset=?")
                .bind(std::fs::metadata(&self.path).unwrap().len() as i64)
                .execute(&mut self.db)
                .await
                .unwrap();
        }
        pub(crate) async fn finish(&mut self, status: &str) {
            sqlx::query("UPDATE thread_turns SET status=?, completed_at=100, rollout_end_byte_offset=?, final_agent_item_id='reply'")
                .bind(status).bind(self.start as i64).execute(&mut self.db).await.unwrap();
            self.indexed().await;
        }

        // Shape accepted and indexed by the installed desktop producer
        // 0.163.0-alpha.10.2 in an offline home: the terminal event has an
        // ordinal and timestamp, but the projected completed_at is NULL.
        pub(crate) async fn producer_completion(&mut self) {
            use std::io::Write;
            let end = std::fs::metadata(&self.path).unwrap().len() as i64;
            writeln!(std::fs::OpenOptions::new().append(true).open(&self.path).unwrap(),
                "{}", serde_json::json!({"timestamp":"2026-10-16T14:00:00.000Z","ordinal":2,
                "type":"event_msg","payload":{"type":"task_complete","turn_id":"turn","last_agent_message":"Synthetic offline reply."}})).unwrap();
            sqlx::query("UPDATE thread_turns SET status='completed', completed_at=NULL, rollout_end_byte_offset=?")
                .bind(end).execute(&mut self.db).await.unwrap();
            self.indexed().await;
        }
    }

    #[tokio::test]
    async fn nullable_terminal_time_requires_complete_matching_rollout_proof() {
        use std::io::Write;
        let mut fixture = Fixture::new().await;
        fixture.producer_completion().await;
        assert_eq!(fixture.read().await.unwrap().status, "completed");
        let completed = std::fs::read(&fixture.path).unwrap();
        let end: i64 = sqlx::query_scalar("SELECT rollout_end_byte_offset FROM thread_turns")
            .fetch_one(&mut fixture.db)
            .await
            .unwrap();
        for payload in [
            serde_json::json!({"type":"task_complete","turn_id":"old-turn"}),
            serde_json::json!({"type":"task_started","turn_id":"turn"}),
            serde_json::json!({"type":"turn_aborted","turn_id":"turn","reason":"interrupted"}),
        ] {
            let mut bytes = completed[..end as usize].to_vec();
            writeln!(
                &mut bytes,
                "{}",
                serde_json::json!({"type":"event_msg","payload":payload})
            )
            .unwrap();
            std::fs::write(&fixture.path, bytes).unwrap();
            fixture.indexed().await;
            assert!(fixture.read().await.is_none(), "{payload}");
        }
        // A complete index cannot make an interrupted JSON line trustworthy.
        std::fs::write(&fixture.path, &completed[..completed.len() - 1]).unwrap();
        fixture.indexed().await;
        assert!(fixture.read().await.is_none());
        std::fs::write(&fixture.path, &completed).unwrap();
        fixture.indexed().await;
        std::fs::OpenOptions::new().append(true).open(&fixture.path).unwrap()
            .write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"next\"}}\n").unwrap();
        assert!(
            fixture.read().await.is_none(),
            "unindexed next generation holds"
        );
        std::fs::write(&fixture.path, &completed).unwrap();
        fixture.indexed().await;
        assert!(
            !fixture.read().await.unwrap().working(),
            "read error/partial write recovery is rechecked"
        );
    }

    #[tokio::test]
    async fn empty_thread_needs_matching_metadata_and_complete_projection() {
        let mut fixture = Fixture::new().await;
        let meta = b"{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"source\":\"vscode\",\"cli_version\":\"0.163.0-alpha.10.2\"}}\n";
        fixture
            .db
            .execute("DELETE FROM thread_turns")
            .await
            .unwrap();
        std::fs::write(&fixture.path, meta).unwrap();
        assert_eq!(
            busy(fixture.dir.path(), "thread", &fixture.path).await,
            None,
            "not-yet-indexed metadata does not admit"
        );
        fixture.indexed().await;
        assert_eq!(
            busy(fixture.dir.path(), "thread", &fixture.path).await,
            Some(false)
        );
        std::fs::write(&fixture.path, [meta.as_slice(), b"{}\n"].concat()).unwrap();
        fixture.indexed().await;
        assert_eq!(
            busy(fixture.dir.path(), "thread", &fixture.path).await,
            None,
            "missing turn rows cannot hide additional records"
        );
        std::fs::write(
            &fixture.path,
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"different\"}}\n",
        )
        .unwrap();
        fixture.indexed().await;
        assert_eq!(
            busy(fixture.dir.path(), "thread", &fixture.path).await,
            None
        );
        std::fs::write(&fixture.path, meta).unwrap();
        fixture.indexed().await;
        fixture
            .db
            .execute("DELETE FROM thread_history_projection_state")
            .await
            .unwrap();
        assert_eq!(
            busy(fixture.dir.path(), "thread", &fixture.path).await,
            None,
            "absent evidence stays unknown"
        );
    }

    #[tokio::test]
    async fn store_identity_and_symlinks_never_reuse_a_completion() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new().await;
        fixture.producer_completion().await;
        let other = tempfile::tempdir().unwrap();
        assert!(busy(other.path(), "thread", &fixture.path).await.is_none());
        let alias = fixture.path.with_file_name("alias-thread.jsonl");
        symlink(&fixture.path, &alias).unwrap();
        assert!(busy(fixture.dir.path(), "thread", &alias).await.is_none());
        assert!(busy(fixture.dir.path(), "different-thread", &fixture.path)
            .await
            .is_none());
        assert!(!fixture.read().await.unwrap().working());
        // An index replaced by rename is read from the new file, never from
        // the kept connection to the old one.
        let database = fixture.dir.path().join("thread_history_1.sqlite");
        let replacement = database.with_extension("new");
        std::fs::copy(&database, &replacement).unwrap();
        let mut copy = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&replacement),
        )
        .await
        .unwrap();
        copy.execute("UPDATE thread_turns SET status='inProgress',completed_at=NULL,rollout_end_byte_offset=NULL")
            .await
            .unwrap();
        copy.close().await.unwrap();
        let (opened, _) = fixture.reader();
        std::fs::rename(&replacement, &database).unwrap();
        assert!(fixture.read().await.unwrap().working());
        assert_eq!(fixture.reader().0, opened + 1);
    }

    #[tokio::test]
    async fn long_turn_uses_indexed_identity_and_matching_terminal_outcome() {
        let mut fixture = Fixture::new().await;
        // The latest turn's start is far outside any former one-megabyte
        // window. A point lookup remains sufficient, including on first read.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&fixture.path)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        fixture.indexed().await;
        assert_eq!(fixture.read().await.unwrap().status, "inProgress");
        fixture.db.execute("INSERT INTO thread_items VALUES('thread','turn','reply','{\"type\":\"agentMessage\",\"text\":\"Finished.\"}')").await.unwrap();
        fixture.finish("completed").await;
        assert_eq!(
            fixture.read().await,
            Some(Activity {
                turn: "turn".into(),
                status: "completed",
                reply: Some("Finished.".into())
            })
        );
        fixture.finish("interrupted").await;
        assert!(!fixture.read().await.unwrap().working());
        assert_eq!(fixture.read().await.unwrap().status, "interrupted");
        // In-place replacement with a different start cannot reuse the index.
        std::fs::write(&fixture.path, b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"different\"}}\n").unwrap();
        assert!(fixture.read().await.is_none());
    }

    #[tokio::test]
    async fn lagging_terminal_index_and_stale_open_turn_are_unknown() {
        use std::io::Write;
        let mut fixture = Fixture::new().await;
        fixture.finish("completed").await;
        let next_start = std::fs::metadata(&fixture.path).unwrap().len();
        std::fs::OpenOptions::new().append(true).open(&fixture.path).unwrap().write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"new-turn\"}}\n").unwrap();
        assert!(
            fixture.read().await.is_none(),
            "a new unindexed turn must hold dispatch"
        );
        sqlx::query(
            "INSERT INTO thread_turns VALUES('thread','new-turn',2,'inProgress',NULL,?,NULL,NULL)",
        )
        .bind(next_start as i64)
        .execute(&mut fixture.db)
        .await
        .unwrap();
        fixture.indexed().await;
        assert_eq!(fixture.read().await.unwrap().turn, "new-turn");
        fixture.db.execute("UPDATE thread_turns SET status='inProgress',completed_at=NULL,rollout_end_byte_offset=NULL").await.unwrap();
        assert!(
            fixture.read().await.unwrap().working(),
            "lagging unfinished evidence remains conservative"
        );
        assert!(read_at(
            fixture.dir.path(),
            "thread",
            &fixture.path,
            SystemTime::now() + MAX_IDLE + Duration::from_secs(1),
            |_| None,
        )
        .await
        .is_none());
        fixture
            .db
            .execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=9999999")
            .await
            .unwrap();
        assert!(fixture.read().await.is_none());
    }

    #[tokio::test]
    async fn corrupt_or_locked_database_and_missing_rows_are_bounded() {
        let mut fixture = Fixture::new().await;
        fixture.db.execute("BEGIN EXCLUSIVE").await.unwrap();
        let started = std::time::Instant::now();
        assert!(fixture.read().await.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        fixture.db.execute("ROLLBACK").await.unwrap();
        assert!(
            fixture.read().await.unwrap().working(),
            "a read after the lock clears answers again"
        );
        fixture
            .db
            .execute("DELETE FROM thread_turns")
            .await
            .unwrap();
        assert!(fixture.read().await.is_none());
        // The reader now holds a spare connection; the file corrupted in place
        // under it is still unknown.
        fixture.db.close().await.unwrap();
        std::fs::write(
            fixture.dir.path().join("thread_history_1.sqlite"),
            "corrupt",
        )
        .unwrap();
        assert!(read(fixture.dir.path(), "thread", &fixture.path)
            .await
            .is_none());
    }

    /// Codex adds migrations on its own schedule (0.163.0 adds realtime items
    /// and root turn ids after the eight 0.162 ones). The reader keys on the
    /// columns it queries: a later schema still answers, a missing reply
    /// table only drops the reply, and an unfinished migration or a missing
    /// lifecycle column is unknown.
    #[tokio::test]
    async fn later_schemas_are_read_by_their_columns_and_degrade_per_feature() {
        type Case = (&'static str, &'static str, Option<Option<&'static str>>);
        let cases: [Case; 7] = [
            ("eight migrations", "", Some(Some("Done."))),
            (
                "later migrations and columns",
                "INSERT INTO _sqlx_migrations VALUES(9,1),(10,1),(11,1);
                 ALTER TABLE thread_turns ADD COLUMN root_turn_id TEXT;
                 ALTER TABLE thread_items ADD COLUMN started_at_ms INTEGER;
                 CREATE TABLE thread_realtime_items(thread_id TEXT, item_id TEXT);",
                Some(Some("Done.")),
            ),
            (
                "no migrations table",
                "DROP TABLE _sqlx_migrations",
                Some(Some("Done.")),
            ),
            ("no reply items", "DROP TABLE thread_items", Some(None)),
            (
                "no final reply column",
                "ALTER TABLE thread_turns DROP COLUMN final_agent_item_id",
                Some(None),
            ),
            (
                "unfinished migration",
                "INSERT INTO _sqlx_migrations VALUES(9,0)",
                None,
            ),
            (
                "missing lifecycle column",
                "ALTER TABLE thread_turns DROP COLUMN rollout_end_byte_offset",
                None,
            ),
        ];
        for (name, change, expected) in cases {
            let mut fixture = Fixture::new().await;
            fixture.finish("completed").await;
            fixture
                .db
                .execute(
                    "INSERT INTO thread_items VALUES('thread','turn','reply','{\"type\":\"agentMessage\",\"text\":\"Done.\"}')",
                )
                .await
                .unwrap();
            // A warm reader holds a connection and a column check from
            // before the change; the change must invalidate the check.
            assert_eq!(
                fixture.read().await.map(|a| a.reply),
                Some(Some("Done.".into())),
                "{name}"
            );
            let (_, checks) = fixture.reader();
            if !change.is_empty() {
                fixture.db.execute(change).await.unwrap();
            }
            let read = fixture.read().await;
            assert_eq!(
                fixture.reader(),
                (1, checks + usize::from(!change.is_empty())),
                "{name}: one connection, checked again only after a change"
            );
            assert_eq!(
                read.as_ref().map(|a| a.reply.as_deref()),
                expected,
                "{name}"
            );
            if let Some(activity) = read {
                assert_eq!(activity.status, "completed", "{name}");
            }
        }
    }

    #[tokio::test]
    async fn replacement_and_unknown_status_are_not_cached_or_written_back() {
        let mut fixture = Fixture::new().await;
        let database = fixture.dir.path().join("thread_history_1.sqlite");
        let before = std::fs::read(&database).unwrap();
        for _ in 0..3 {
            assert!(fixture.read().await.unwrap().working());
        }
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(
            fixture.reader(),
            (1, 1),
            "repeated reads share one connection and one column check"
        );
        fixture
            .db
            .execute("UPDATE thread_turns SET status='futureState'")
            .await
            .unwrap();
        assert!(fixture.read().await.is_none());
        fixture
            .db
            .execute("UPDATE thread_turns SET status='inProgress'")
            .await
            .unwrap();
        fixture
            .db
            .execute("UPDATE thread_turns SET turn_id='different'")
            .await
            .unwrap();
        assert!(
            fixture.read().await.is_none(),
            "an offset from a different turn is not evidence"
        );
        fixture
            .db
            .execute("UPDATE thread_turns SET turn_id='turn'")
            .await
            .unwrap();
        let replacement = fixture.path.with_extension("new");
        std::fs::write(&replacement, b"different file").unwrap();
        std::fs::rename(replacement, &fixture.path).unwrap();
        assert!(fixture.read().await.is_none());
        let outside = fixture.dir.path().join("rollout-other-thread.jsonl");
        std::fs::write(&outside, b"outside sessions").unwrap();
        assert!(read(fixture.dir.path(), "thread", &outside).await.is_none());
    }

    #[tokio::test]
    async fn a_quiet_open_turn_runs_only_while_its_earlier_writer_lives() {
        use std::io::Write;
        let mut fixture = Fixture::new().await;
        // The turn's start record as Codex writes it, with its timestamp.
        let prefix = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\"}}\n";
        std::fs::write(&fixture.path, format!("{prefix}{{\"timestamp\":\"2026-10-14T10:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn\"}}}}\n")).unwrap();
        fixture.indexed().await;
        // Warm: the reader answers running within MAX_IDLE first, so the
        // reads below cross the quiet threshold on a reused connection.
        assert!(fixture.read().await.unwrap().working());
        let late = SystemTime::now() + MAX_IDLE + Duration::from_secs(60);
        let at = |writer: fn(&std::fs::Metadata) -> Option<SystemTime>| {
            read_at(fixture.dir.path(), "thread", &fixture.path, late, writer)
        };
        // Codex has had the rollout open since before the turn: still running.
        let running = at(|_| Some(SystemTime::UNIX_EPOCH)).await.unwrap();
        assert_eq!(
            (running.turn.as_str(), running.status),
            ("turn", "inProgress")
        );
        // No live writer, or one that started after the turn (a later Codex
        // that reopened an abandoned turn): unknown, never idle. The writer
        // exiting right after a running answer is not hidden by reuse.
        assert!(at(|_| None).await.is_none());
        let after = |_: &std::fs::Metadata| Some(SystemTime::now());
        assert!(at(after).await.is_none());
        // An index behind the rollout cannot vouch for the open turn.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.path)
            .unwrap()
            .write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\"}}\n")
            .unwrap();
        assert!(at(|_| Some(SystemTime::UNIX_EPOCH)).await.is_none());
        // Within MAX_IDLE the open turn needs no writer evidence.
        fixture.indexed().await;
        assert!(fixture.read().await.unwrap().working());
        assert_eq!(fixture.reader().0, 1);
    }

    #[test]
    fn one_scan_answers_a_window_even_when_it_does_not_finish() {
        let scans = std::cell::Cell::new(0);
        let failing = || {
            scans.set(scans.get() + 1);
            None
        };
        let window = Scans::new(Duration::from_secs(3600));
        for _ in 0..3 {
            assert!(window.writers(failing).is_none());
        }
        assert_eq!(scans.get(), 1, "a failed scan is not repeated per row");
        let expired = Scans::new(Duration::ZERO);
        for _ in 0..2 {
            assert!(expired
                .writers(|| {
                    scans.set(scans.get() + 1);
                    Some(Writers::from([((1, 2), SystemTime::UNIX_EPOCH)]))
                })
                .is_some_and(|writers| writers.contains_key(&(1, 2))));
        }
        assert_eq!(scans.get(), 3, "an expired window scans again");
    }

    #[tokio::test(start_paused = true)]
    async fn unused_connections_close_and_the_reaper_ends() {
        let fixture = Fixture::new().await;
        let reader = Arc::new(Reader {
            state: Mutex::default(),
        });
        let connect = || async {
            sqlx::SqliteConnection::connect_with(
                &sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(fixture.dir.path().join("thread_history_1.sqlite"))
                    .read_only(true),
            )
            .await
            .unwrap()
        };
        // Connect first: paused time advances while a connect is awaited.
        let connections = [connect().await, connect().await, connect().await];
        let replaced = connect().await;
        for connection in connections {
            reader.put((1, 1), connection);
        }
        assert_eq!(reader.state().spare.len(), MAX_SPARE_CONNECTIONS);
        assert!(reader.state().reaping);
        // A connection to a replaced index is not kept beside current ones.
        reader.put((1, 2), replaced);
        assert_eq!(reader.state().spare.len(), MAX_SPARE_CONNECTIONS);
        tokio::time::sleep(CONNECTION_IDLE * 3).await;
        let state = reader.state();
        assert!(state.spare.is_empty());
        assert!(!state.reaping, "the reaper ends with no connections left");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_file_held_open_for_writing_names_its_writer() {
        let dir = tempfile::tempdir().unwrap();
        let written = dir.path().join("written.jsonl");
        let read_only = dir.path().join("read.jsonl");
        std::fs::write(&read_only, b"").unwrap();
        let _writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&written)
            .unwrap();
        let _reader = std::fs::File::open(&read_only).unwrap();
        // One scan serves a second, so wait out a scan another test started. A
        // scan that misses its deadline on a loaded machine answers None for
        // that window, so try a few windows before calling the writer unseen.
        let mut writer = None;
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(1_100));
            writer = live_writer_started(&std::fs::metadata(&written).unwrap());
            if writer.is_some() {
                break;
            }
        }
        assert!(writer.is_some_and(|at| at <= SystemTime::now()));
        assert!(live_writer_started(&std::fs::metadata(&read_only).unwrap()).is_none());
    }
}
