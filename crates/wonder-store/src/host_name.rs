//! The computer's name in Wonder (`host_name`). One owner: the host stores it
//! and the Mac app and the phone both read and change it here.

use crate::Store;
use sqlx::Row;

/// The longest name, in characters (the table checks it too).
pub const HOST_NAME_MAX_CHARS: usize = 60;

impl Store {
    /// The name the owner chose, if any.
    pub async fn host_name(&self) -> Result<Option<String>, sqlx::Error> {
        Ok(sqlx::query("SELECT name FROM host_name WHERE id=1")
            .fetch_optional(&self.pool)
            .await?
            .map(|row| row.get("name")))
    }

    /// Saves a chosen name, or with `None` goes back to the computer's own.
    pub async fn set_host_name(&self, name: Option<&str>, now: &str) -> Result<(), sqlx::Error> {
        match name {
            Some(name) => {
                sqlx::query(
                    "INSERT INTO host_name(id,name,updated_at) VALUES(1,?,?) \
                     ON CONFLICT(id) DO UPDATE SET name=excluded.name,updated_at=excluded.updated_at",
                )
                .bind(name)
                .bind(now)
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM host_name WHERE id=1")
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Contract: no name until one is chosen; a chosen name replaces the last
    // and clearing it goes back to none. An empty name is refused by the table.
    #[tokio::test]
    async fn chosen_name_persists_and_clears() {
        let store = crate::Store::connect("sqlite::memory:").await.unwrap();
        assert_eq!(store.host_name().await.unwrap(), None);
        store.set_host_name(Some("Studio"), "t1").await.unwrap();
        store.set_host_name(Some("Work Mac"), "t2").await.unwrap();
        assert_eq!(
            store.host_name().await.unwrap().as_deref(),
            Some("Work Mac")
        );
        assert!(store.set_host_name(Some("  "), "t3").await.is_err());
        store.set_host_name(None, "t4").await.unwrap();
        assert_eq!(store.host_name().await.unwrap(), None);
    }
}
