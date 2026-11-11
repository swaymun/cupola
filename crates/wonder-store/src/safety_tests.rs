//! Storage fault contracts use disposable databases, real SQLite errors and
//! killed child processes. No owner database or running daemon is involved.
use super::*;
use std::path::Path;

fn database_url(path: &Path) -> String {
    format!("sqlite://{}?mode=rwc", path.display())
}

async fn single_connection_store(path: &Path) -> Store {
    let url = database_url(path);
    Store::connect(&url).await.unwrap().pool.close().await;
    Store {
        pool: SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap(),
    }
}

async fn seed_device(store: &Store) {
    store
        .upsert_owner_device("phone", "Phone", "{}", "now")
        .await
        .unwrap();
    store
        .insert_session("session", "phone", "csrf", 4_102_444_800_000, "now")
        .await
        .unwrap();
}

async fn device_state(store: &Store) -> (bool, i64) {
    sqlx::query_as("SELECT revoked_at IS NOT NULL, (SELECT COUNT(*) FROM sessions WHERE device_id='phone') FROM devices WHERE id='phone'")
        .fetch_one(&store.pool).await.unwrap()
}

async fn integrity(store: &Store) {
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
            .fetch_all(&store.pool)
            .await
            .unwrap(),
        ["ok"]
    );
    assert!(sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&store.pool)
        .await
        .unwrap()
        .is_empty());
}

// Contract: every retained schema, including the oldest populated schema,
// opens through the production upgrader without losing durable identity/data.
#[tokio::test]
async fn every_historical_schema_upgrades_to_head() {
    let dir = tempfile::tempdir().unwrap();
    let source = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&database_url(&dir.path().join("history.db")))
        .await
        .unwrap();
    sqlx::query("PRAGMA auto_vacuum=INCREMENTAL")
        .execute(&source)
        .await
        .unwrap();
    let migrations = sqlx::migrate!();
    for count in 0..=migrations.iter().count() {
        let prefix = migrations.iter().take(count).cloned().collect::<Vec<_>>();
        let version = prefix.last().map_or(0, |m| m.version);
        sqlx::migrate::Migrator::with_migrations(prefix)
            .run(&source)
            .await
            .unwrap();
        if version == 1 {
            sqlx::raw_sql("INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES ('old-phone','Old phone','owner','{}','then');
                INSERT INTO sessions VALUES ('old-session','old-phone','old-csrf',4102444800000,'then');
                INSERT INTO messages(id,device_id,client_message_id,body_sha256,conversation_id,state,created_at) VALUES ('old-message','old-phone','old-client','old-hash','legacy','completed','then');")
                .execute(&source).await.unwrap();
        }
        if version == 2 {
            sqlx::query("UPDATE messages SET body='Preserve the oldest message'")
                .execute(&source)
                .await
                .unwrap();
        }
        if version == 3 {
            sqlx::query("INSERT INTO conversations(id,created_at) VALUES ('legacy','then')")
                .execute(&source)
                .await
                .unwrap();
        }
        let path = dir.path().join(format!("schema-{version}.db"));
        sqlx::query("VACUUM INTO ?")
            .bind(path.to_str().unwrap())
            .execute(&source)
            .await
            .unwrap();
        let upgraded = Store::connect(&database_url(&path))
            .await
            .unwrap_or_else(|e| panic!("schema {version} failed to upgrade: {e}"));
        let applied: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success=1")
                .fetch_one(&upgraded.pool)
                .await
                .unwrap();
        assert_eq!(
            applied as usize,
            migrations.iter().count(),
            "schema {version}"
        );
        if version >= 1 {
            let message = upgraded
                .message_by_id("old-message")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                message.body,
                if version >= 2 {
                    "Preserve the oldest message"
                } else {
                    ""
                },
                "schema {version}"
            );
            assert_eq!(message.body_sha256, "old-hash");
            assert_eq!(message.device_id, "old-phone");
            assert_eq!(upgraded.list_active_sessions(0).await.unwrap().len(), 1);
        }
        integrity(&upgraded).await;
        upgraded.pool.close().await;
        let reopened = Store::connect(&database_url(&path)).await.unwrap();
        reopened
            .set_host_name(Some("After upgrade"), "now")
            .await
            .unwrap();
        assert_eq!(
            reopened.host_name().await.unwrap().as_deref(),
            Some("After upgrade")
        );
        reopened.pool.close().await;
        std::fs::remove_file(path).unwrap();
    }
    eprintln!(
        "STORAGE_MIGRATIONS: empty plus {} retained schemas upgraded, reopened and checked",
        migrations.iter().count()
    );
    source.close().await;
}

// SQLx's SQLite migration lock is a no-op; opens must serialize the entire
// read-applied/apply sequence, not only each individual migration statement.
#[tokio::test]
async fn concurrent_openers_upgrade_once() {
    let head = sqlx::migrate!().iter().last().unwrap().version;
    for version in [0, 1, 114, head] {
        let dir = tempfile::tempdir().unwrap();
        let url = database_url(&dir.path().join("concurrent.db"));
        if version != 0 {
            let old = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            sqlx::query("PRAGMA journal_mode=WAL")
                .execute(&old)
                .await
                .unwrap();
            sqlx::migrate::Migrator::with_migrations(
                sqlx::migrate!()
                    .iter()
                    .filter(|m| m.version <= version)
                    .cloned()
                    .collect(),
            )
            .run(&old)
            .await
            .unwrap();
            old.close().await;
        }
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(6));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let (url, barrier) = (url.clone(), barrier.clone());
            tasks.spawn(async move {
                barrier.wait().await;
                Store::connect(&url).await
            });
        }
        let mut errors = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(store) => store.pool.close().await,
                Err(error) => errors.push(error.to_string()),
            }
        }
        assert!(
            errors.is_empty(),
            "schema {version}: concurrent open failures: {errors:?}"
        );
        let store = Store::connect(&url).await.unwrap();
        integrity(&store).await;
        store.pool.close().await;
    }
}

#[tokio::test]
async fn startup_waits_for_a_legacy_journal_reader() {
    let dir = tempfile::tempdir().unwrap();
    let url = database_url(&dir.path().join("reader.db"));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate::Migrator::with_migrations(sqlx::migrate!().iter().take(1).cloned().collect())
        .run(&pool)
        .await
        .unwrap();
    let mut reader = pool.begin().await.unwrap();
    sqlx::query("SELECT * FROM devices")
        .fetch_all(&mut *reader)
        .await
        .unwrap();
    let opening = tokio::spawn(async move { Store::connect(&url).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    reader.commit().await.unwrap();
    let opened = opening.await.unwrap().unwrap();
    opened.pool.close().await;
    pool.close().await;
}

// A genuine SQLITE_FULL on the last write must not leave a revoked device
// with only some of its credentials removed. Retrying must be well-defined.
#[tokio::test]
async fn disk_full_revocation_rolls_back_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let store = single_connection_store(&dir.path().join("full.db")).await;
    seed_device(&store).await;
    sqlx::raw_sql("CREATE TABLE full_probe(payload BLOB); CREATE TRIGGER fill_on_revoke AFTER DELETE ON sessions BEGIN INSERT INTO full_probe VALUES(zeroblob(2097152)); END;")
        .execute(&store.pool).await.unwrap();
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA max_page_count={}",
        pages + 1
    )))
    .execute(&store.pool)
    .await
    .unwrap();
    let error = store
        .revoke_owner_device("phone", "later")
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("13"),
        "must exercise SQLITE_FULL: {error}"
    );
    assert_eq!(
        device_state(&store).await,
        (false, 1),
        "a failed revocation must leave the original state"
    );
    sqlx::raw_sql("DROP TRIGGER fill_on_revoke; PRAGMA max_page_count=1073741823;")
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(store.revoke_owner_device("phone", "later").await.unwrap());
    assert_eq!(device_state(&store).await, (true, 0));
    assert!(!store.revoke_owner_device("phone", "again").await.unwrap());
    integrity(&store).await;
    store.pool.close().await;
}

// Rust decoding must return an error, never panic or reinterpret a negative
// expiration as an enormous future authorization.
#[tokio::test]
async fn corrupted_auth_rows_fail_closed_without_panicking() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_device(&store).await;
    sqlx::query("UPDATE sessions SET expires_at_ms='not an integer'")
        .execute(&store.pool)
        .await
        .unwrap();
    let task_store = store.clone();
    let result = tokio::spawn(async move { task_store.list_active_sessions(0).await }).await;
    assert!(result.is_ok(), "corrupt session row panicked: {result:?}");
    assert!(
        result.unwrap().is_err(),
        "corrupt sessions must fail closed"
    );
    for value in ["-1", "not an integer"] {
        sqlx::query("UPDATE devices SET session_expires_at_ms=?")
            .bind(value)
            .execute(&store.pool)
            .await
            .unwrap();
        let task_store = store.clone();
        let result = tokio::spawn(async move { task_store.list_owner_devices().await }).await;
        assert!(result.is_ok(), "corrupt device row panicked: {result:?}");
        assert!(
            result.unwrap().is_err(),
            "corrupt device expiration {value} must fail closed"
        );
    }
}

// Read-before-write operations must wait for an independent writer instead
// of trying to upgrade a stale WAL snapshot (SQLITE_BUSY ignores busy_timeout
// in that case). All four paths previously began deferred transactions.
#[tokio::test]
async fn read_then_write_operations_wait_for_concurrent_writer() {
    let mut failures = Vec::new();
    for operation in [
        "reset",
        "approval event",
        "push registration",
        "push revocation",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&database_url(&dir.path().join("writers.db")))
            .await
            .unwrap();
        seed_device(&store).await;
        store.start_event_epoch("epoch").await.unwrap();
        store
            .register_push(
                "phone",
                "registration",
                "https://example.invalid",
                "fixture",
            )
            .await
            .unwrap();
        if operation == "push revocation" {
            sqlx::query("UPDATE push_registrations SET revoked=1")
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let mut writer = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("INSERT INTO action_nonces VALUES ('independent-writer','now')")
            .execute(&mut *writer)
            .await
            .unwrap();
        let worker = tokio::spawn({
            let store = store.clone();
            async move {
                match operation {
                    "reset" => store.reset_workspace("phone", "later").await.map(|_| ()),
                    "approval event" => {
                        let mut event: HostEventEnvelope = serde_json::from_value(serde_json::json!({
                            "eventId":"resolved", "hostEpoch":"epoch", "sequence":0, "occurredAt":"now",
                            "event":{"type":"approval_resolved", "data":{"request_id":"missing", "decision":"approved"}}
                        })).unwrap();
                        store.commit_event(&mut event).await
                    }
                    "push registration" => {
                        store
                            .register_push(
                                "phone",
                                "new-registration",
                                "https://example.invalid",
                                "new-fixture",
                            )
                            .await
                    }
                    "push revocation" => store.next_push_revocation().await.map(|value| {
                        assert!(value.is_some());
                    }),
                    _ => unreachable!(),
                }
            }
        });
        // Match the existing turn-completion contention fixture. This holds
        // the writer briefly, without an elapsed-time pass/fail requirement.
        tokio::time::sleep(Duration::from_millis(300)).await;
        writer.commit().await.unwrap();
        if let Err(error) = worker.await.unwrap() {
            failures.push(format!("{operation}: {error}"));
        }
        integrity(&store).await;
        store.pool.close().await;
    }
    assert!(
        failures.is_empty(),
        "read/write contention failures: {failures:?}"
    );
}

// Replay is disposable; a corrupt envelope must ask the client for a fresh
// snapshot instead of failing every reconnect until the replay row expires.
#[tokio::test]
async fn corrupt_replay_requests_resync_without_touching_canonical_data() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_device(&store).await;
    store.start_event_epoch("epoch").await.unwrap();
    let MessageInsert::Inserted(message) = store
        .insert_message("phone", "receipt", "Canonical text", "hash", "chat", "now")
        .await
        .unwrap()
    else {
        panic!("new message")
    };
    let journal_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_journal")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert!(journal_rows > 0);
    for corrupt in [
        "payload_json='{'",
        "payload_json='{}'",
        "payload_json='[]'",
        "payload_json=x'ff'",
        "resource=x'ff'",
        "occurred_at=x'ff'",
        "conversation_id=x'ff'",
        "change_kind=x'ff'",
    ] {
        sqlx::query("UPDATE sync_journal SET payload_json=NULL, resource='messages', occurred_at='now', conversation_id='chat', change_kind='insert'")
            .execute(&store.pool).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE sync_journal SET {corrupt}"
        )))
        .execute(&store.pool)
        .await
        .unwrap();
        let task_store = store.clone();
        let result = tokio::spawn(async move { task_store.replay_batch("epoch", 0).await }).await;
        assert!(
            result.is_ok(),
            "corrupt replay row panicked: {corrupt}: {result:?}"
        );
        assert!(
            matches!(
                result.unwrap().unwrap(),
                ReplayBatch::Resync(wonder_api::ResyncReason::ReplayWindowExpired)
            ),
            "bad replay {corrupt}"
        );
        assert_eq!(
            store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .body,
            "Canonical text"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM sync_journal WHERE {corrupt}"
            )))
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            journal_rows,
            "retain the corrupt evidence"
        );
    }
}

// An unreadable legacy row must fail the upgrade transaction without
// deleting evidence or leaving a half-created index/table. An explicit repair
// of that row allows the unchanged historical migration to run on retry.
#[tokio::test]
async fn failed_legacy_migration_preserves_data_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let url = database_url(&dir.path().join("bad-legacy.db"));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate::Migrator::with_migrations(
        sqlx::migrate!()
            .iter()
            .filter(|m| m.version <= 35)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO events VALUES ('corrupt','epoch',1,'4102444800000','{')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(Store::connect(&url).await.is_err());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        35
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='history_entries'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT payload_json FROM events WHERE event_id='corrupt'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "{"
    );
    sqlx::query("UPDATE events SET payload_json='{}' WHERE event_id='corrupt'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let store = Store::connect(&url).await.unwrap();
    integrity(&store).await;
    store.pool.close().await;
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn kill_at_boundary(path: &Path, ready: &Path, boundary: &str) {
    let mut child = KillOnDrop(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "safety_tests::killed_writer_child",
                "--nocapture",
            ])
            .env("WONDER_STORE_KILL_DB", path)
            .env("WONDER_STORE_KILL_READY", ready)
            .env("WONDER_STORE_KILL_BOUNDARY", boundary)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before {boundary}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never reached the write boundary");
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}

// Interrupt the actual upgrader after DDL/data writes but before its receipt
// commits, including the history and provider-table rebuilds. A reopen must
// finish the upgrade exactly once and preserve the oldest durable records.
#[tokio::test]
async fn killed_migrations_roll_back_and_reopen_from_oldest_schema() {
    for version in [2, 36, 115] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migration.db");
        let url = database_url(&path);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate::Migrator::with_migrations(
            sqlx::migrate!().iter().take(1).cloned().collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql("INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES ('old-phone','Old phone','owner','{}','then');
            INSERT INTO messages(id,device_id,client_message_id,body_sha256,conversation_id,state,created_at) VALUES ('old-message','old-phone','old-client','old-hash','legacy','completed','then');")
            .execute(&pool).await.unwrap();
        pool.close().await;
        kill_at_boundary(
            &path,
            &dir.path().join("ready"),
            &format!("migration:{version}"),
        )
        .await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "upgrade must roll back after kill at {version}"
        );
        pool.close().await;
        let upgraded = Store::connect(&url).await.unwrap();
        let message = upgraded
            .message_by_id("old-message")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.body_sha256, "old-hash");
        assert_eq!(message.device_id, "old-phone");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&upgraded.pool)
                .await
                .unwrap() as usize,
            sqlx::migrate!().iter().count()
        );
        integrity(&upgraded).await;
        upgraded.pool.close().await;
    }
}

// An update hook pauses inside the real second write. SIGKILL loses
// uncommitted pages; reopening must see exactly the pre- or post-commit state.
#[tokio::test]
async fn killed_revocation_is_atomic_on_reopen() {
    for boundary in ["sessions", "committed"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kill.db");
        let ready = dir.path().join("ready");
        let store = Store::connect(&database_url(&path)).await.unwrap();
        seed_device(&store).await;
        store.pool.close().await;
        kill_at_boundary(&path, &ready, boundary).await;
        let reopened = Store::connect(&database_url(&path)).await.unwrap();
        assert_eq!(
            device_state(&reopened).await,
            if boundary == "committed" {
                (true, 0)
            } else {
                (false, 1)
            },
            "killed at {boundary}"
        );
        integrity(&reopened).await;
        if boundary != "committed" {
            assert!(reopened
                .revoke_owner_device("phone", "retry")
                .await
                .unwrap());
            assert_eq!(device_state(&reopened).await, (true, 0));
        }
        reopened.pool.close().await;
    }
}

#[tokio::test]
async fn killed_writer_child() {
    let Some(path) = std::env::var_os("WONDER_STORE_KILL_DB") else {
        return;
    };
    let ready = std::path::PathBuf::from(std::env::var_os("WONDER_STORE_KILL_READY").unwrap());
    let boundary = std::env::var("WONDER_STORE_KILL_BOUNDARY").unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&database_url(Path::new(&path)))
        .await
        .unwrap();
    let migration_version = boundary
        .strip_prefix("migration:")
        .map(|v| v.parse::<i64>().unwrap());
    if boundary != "committed" {
        let mut connection = pool.acquire().await.unwrap();
        let mut handle = connection.lock_handle().await.unwrap();
        let signal = ready.clone();
        let mut migrations_left = migration_version.map_or(0, |version| {
            sqlx::migrate!()
                .iter()
                .filter(|m| m.version > 1 && m.version <= version)
                .count()
        });
        handle.set_update_hook(move |change| {
            let pause = if migration_version.is_some() {
                if change.table == "_sqlx_migrations"
                    && matches!(change.operation, sqlx::sqlite::SqliteOperation::Insert)
                {
                    migrations_left -= 1;
                    migrations_left == 0
                } else {
                    false
                }
            } else {
                change.table == boundary
            };
            if pause {
                std::fs::write(&signal, b"paused").unwrap();
                std::thread::sleep(Duration::from_secs(90));
            }
        });
    }
    let store = Store { pool };
    if migration_version.is_some() {
        store.migrate().await.unwrap();
    } else {
        store.revoke_owner_device("phone", "revoked").await.unwrap();
    }
    std::fs::write(ready, b"committed").unwrap();
    tokio::time::sleep(Duration::from_secs(90)).await;
    panic!("parent did not kill the writer");
}
