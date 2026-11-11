//! Agent pictures kept by reference (see migration 0130): the place on this
//! Mac a preview's original bytes are read from. wonderd verifies every read
//! against the file record's size and SHA-256; this module only stores.

use crate::Store;
use sqlx::Row;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaSourceLocation {
    /// The image file the tool showed.
    File { path: String },
    /// Base64 text at `[offset, offset + length)` of a transcript.
    Transcript {
        path: String,
        offset: u64,
        length: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaSource {
    pub location: MediaSourceLocation,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Store {
    /// Records a referenced preview: its file record (no relative path) and
    /// its source, together. Returns the persisted `updated_at`, which an
    /// unchanged replay keeps.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_referenced_file(
        &self,
        id: &str,
        conversation_id: &str,
        name: &str,
        mime_type: &str,
        byte_size: i64,
        sha256: &str,
        source_id: &str,
        source: &MediaSource,
        now: &str,
    ) -> Result<String, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let updated_at: String = sqlx::query_scalar("INSERT INTO conversation_files (id, conversation_id, kind, name, mime_type, byte_size, sha256, relative_path, state, source_id, created_at, updated_at) VALUES (?, ?, 'attachment', ?, ?, ?, ?, NULL, 'available', ?, ?, ?) ON CONFLICT(id) DO UPDATE SET name = excluded.name, mime_type = excluded.mime_type, byte_size = excluded.byte_size, sha256 = excluded.sha256, relative_path = NULL, state = 'available',
            updated_at = CASE WHEN
                (conversation_files.name, conversation_files.mime_type, conversation_files.byte_size, conversation_files.sha256, conversation_files.relative_path, conversation_files.state)
                IS (excluded.name, excluded.mime_type, excluded.byte_size, excluded.sha256, NULL, 'available')
                THEN conversation_files.updated_at ELSE excluded.updated_at END
            RETURNING updated_at")
            .bind(id).bind(conversation_id).bind(name).bind(mime_type).bind(byte_size).bind(sha256)
            .bind(source_id).bind(now).bind(now)
            .fetch_one(&mut *transaction)
            .await?;
        let (kind, path, offset, length) = match &source.location {
            MediaSourceLocation::File { path } => ("file", path, None, None),
            MediaSourceLocation::Transcript {
                path,
                offset,
                length,
            } => (
                "transcript",
                path,
                Some(*offset as i64),
                Some(*length as i64),
            ),
        };
        sqlx::query("INSERT INTO conversation_file_sources (file_id, kind, path, offset, length, width, height, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(file_id) DO UPDATE SET kind = excluded.kind, path = excluded.path, offset = excluded.offset, length = excluded.length, width = excluded.width, height = excluded.height")
            .bind(id).bind(kind).bind(path).bind(offset).bind(length)
            .bind(source.width.map(i64::from)).bind(source.height.map(i64::from)).bind(now)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(updated_at)
    }

    /// Where a referenced preview's original lives; `None` for a stored copy.
    pub async fn media_source(&self, file_id: &str) -> Result<Option<MediaSource>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT kind, path, offset, length, width, height FROM conversation_file_sources WHERE file_id = ?",
        )
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| media_source(&row)))
    }

    /// Every conversation file ID, for the preview-folder orphan report.
    pub async fn conversation_file_ids(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT id FROM conversation_files")
            .fetch_all(&self.pool)
            .await
    }
}

fn media_source(row: &sqlx::sqlite::SqliteRow) -> Option<MediaSource> {
    let path: String = row.get("path");
    let location = match row.get::<String, _>("kind").as_str() {
        "file" => MediaSourceLocation::File { path },
        "transcript" => MediaSourceLocation::Transcript {
            path,
            offset: u64::try_from(row.get::<Option<i64>, _>("offset")?).ok()?,
            length: u64::try_from(row.get::<Option<i64>, _>("length")?).ok()?,
        },
        _ => return None,
    };
    let dimension = |name: &str| {
        row.get::<Option<i64>, _>(name)
            .and_then(|value| u32::try_from(value).ok())
    };
    Some(MediaSource {
        location,
        width: dimension("width"),
        height: dimension("height"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        );
        (dir, Store::connect(&url).await.unwrap())
    }

    /// A referenced preview is a file record without a stored copy plus its
    /// source; replaying it keeps its clock; removing the record removes the
    /// source; a transcript source must say where its text is.
    #[tokio::test]
    async fn referenced_files_keep_their_source_with_the_record() {
        let (_dir, store) = store().await;
        let source = MediaSource {
            location: MediaSourceLocation::Transcript {
                path: "/Users/me/.claude/projects/p/s.jsonl".into(),
                offset: 120,
                length: 4096,
            },
            width: Some(1600),
            height: Some(900),
        };
        let first = store
            .upsert_referenced_file(
                "f1",
                "c1",
                "Tool result.png",
                "image/png",
                3072,
                "ab",
                "tool-media:t:i:0",
                &source,
                "t1",
            )
            .await
            .unwrap();
        let again = store
            .upsert_referenced_file(
                "f1",
                "c1",
                "Tool result.png",
                "image/png",
                3072,
                "ab",
                "tool-media:t:i:0",
                &source,
                "t2",
            )
            .await
            .unwrap();
        assert_eq!((first.as_str(), again.as_str()), ("t1", "t1"));
        let files = store.list_conversation_files("c1").await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, None);
        assert_eq!(files[0].sha256.as_deref(), Some("ab"));
        assert_eq!(store.media_source("f1").await.unwrap(), Some(source));
        assert_eq!(
            store.conversation_file_ids().await.unwrap(),
            vec!["f1".to_owned()]
        );
        assert!(sqlx::query("INSERT INTO conversation_file_sources (file_id, kind, path, created_at) VALUES ('f1x', 'transcript', '/t', 'now')")
            .execute(&store.pool)
            .await
            .is_err());
        sqlx::query("DELETE FROM conversation_files WHERE id = 'f1'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.media_source("f1").await.unwrap(), None);
    }

    async fn file_updates_journaled(store: &Store) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM sync_journal WHERE resource = 'conversation_files' AND change_kind = 'update'")
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    /// Replaying an unchanged preview journals nothing, so phones are not
    /// woken; a real change, or a change to any column at all, still does.
    #[tokio::test]
    async fn only_real_file_changes_wake_phones() {
        let (_dir, store) = store().await;
        store.start_event_epoch("epoch").await.unwrap();
        let source = MediaSource {
            location: MediaSourceLocation::File {
                path: "/tmp/shot.png".into(),
            },
            width: None,
            height: None,
        };
        let save = |sha: &'static str, now: &'static str| {
            let store = &store;
            let source = &source;
            async move {
                store
                    .upsert_referenced_file(
                        "f1",
                        "c1",
                        "shot.png",
                        "image/png",
                        10,
                        sha,
                        "tool-media:t:i:0",
                        source,
                        now,
                    )
                    .await
                    .unwrap()
            }
        };
        save("ab", "t1").await;
        save("ab", "t2").await;
        save("ab", "t3").await;
        assert_eq!(file_updates_journaled(&store).await, 0);
        assert_eq!(save("cd", "t4").await, "t4");
        assert_eq!(file_updates_journaled(&store).await, 1);

        sqlx::query("INSERT INTO conversation_files (id, conversation_id, kind, name, state, created_at, updated_at) VALUES ('f2', 'c1', 'attachment', 'a.txt', 'available', 't', 't')")
            .execute(&store.pool)
            .await
            .unwrap();
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('conversation_files')")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(columns.len() >= 14);
        let mut id = "f2".to_owned();
        for column in columns {
            let before = file_updates_journaled(&store).await;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE conversation_files SET {column} = COALESCE({column}, '') || 'x' WHERE id = ?"
            )))
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
            if column == "id" {
                id.push('x');
            }
            assert_eq!(
                file_updates_journaled(&store).await,
                before + 1,
                "changing {column} must journal an update"
            );
        }
    }
}
