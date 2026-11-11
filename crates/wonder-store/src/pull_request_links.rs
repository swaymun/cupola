//! Pull requests a chat opened through Wonder's Git actions. The repository
//! path stays on the Mac, where the chat's repository index reads it.

use crate::Store;
use sqlx::Row;

/// At most this many links are kept per chat; older ones are dropped.
const MAX_LINKS_PER_CONVERSATION: i64 = 20;

impl Store {
    /// Links a pull request to the chat that opened it, newest kept.
    pub async fn link_thread_pull_request(
        &self,
        conversation: &str,
        url: &str,
        repository_path: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO thread_pull_request_links(conversation_id,url,repository_path,created_at) VALUES(?,?,?,?) ON CONFLICT(conversation_id,url) DO UPDATE SET repository_path=excluded.repository_path")
            .bind(conversation)
            .bind(url)
            .bind(repository_path)
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM thread_pull_request_links WHERE conversation_id=?1 AND url NOT IN (SELECT url FROM thread_pull_request_links WHERE conversation_id=?1 ORDER BY created_at DESC, url LIMIT ?2)")
            .bind(conversation)
            .bind(MAX_LINKS_PER_CONVERSATION)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The chat's linked pull requests as (url, repository path), newest first.
    pub async fn thread_pull_request_links(
        &self,
        conversation: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT url, repository_path FROM thread_pull_request_links WHERE conversation_id=? ORDER BY created_at DESC, url")
            .bind(conversation)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|row| (row.get("url"), row.get("repository_path")))
            .collect())
    }
}
