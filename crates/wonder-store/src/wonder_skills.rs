//! The owner's switches for Wonder's own skill pack, and whether a message
//! was written on a paired phone. Every skill is off until the owner turns it
//! on; a missing row is off.

use crate::Store;
use sqlx::Row;

impl Store {
    /// The skills the owner turned on, by id, in id order.
    pub async fn enabled_wonder_skills(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT skill FROM wonder_skill_settings ORDER BY skill")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get("skill")).collect())
    }

    /// Turning a skill off removes its row, returning it to the default.
    pub async fn set_wonder_skill(
        &self,
        skill: &str,
        enabled: bool,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        if enabled {
            sqlx::query("INSERT INTO wonder_skill_settings(skill,enabled,updated_at) VALUES(?,1,?) ON CONFLICT(skill) DO UPDATE SET updated_at=excluded.updated_at")
                .bind(skill)
                .bind(now)
                .execute(&self.pool)
                .await?;
        } else {
            sqlx::query("DELETE FROM wonder_skill_settings WHERE skill=?")
                .bind(skill)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Whether the owner wrote this message on a paired phone or tablet: its
    /// device is a paired (not this Mac's local) owner device, and it is not
    /// an agent's message (those carry the caller's device) or an automation
    /// run.
    pub async fn message_from_paired_device(&self, message_id: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages m JOIN devices d ON d.id=m.device_id
             WHERE m.id=? AND d.role='owner' AND d.is_local=0
             AND m.client_message_id NOT LIKE 'automation:%'
             AND NOT EXISTS(SELECT 1 FROM message_sources s WHERE s.message_id=m.id))",
        )
        .bind(message_id)
        .fetch_one(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageInsert, NewMessageSource};

    // Contract: skills are off until turned on, a switch persists, and turning
    // it off returns to the default.
    #[tokio::test]
    async fn switches_default_off_and_persist() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert!(store.enabled_wonder_skills().await.unwrap().is_empty());
        store.set_wonder_skill("b", true, "t1").await.unwrap();
        store.set_wonder_skill("a", true, "t1").await.unwrap();
        store.set_wonder_skill("a", true, "t2").await.unwrap();
        assert_eq!(store.enabled_wonder_skills().await.unwrap(), ["a", "b"]);
        store.set_wonder_skill("b", false, "t3").await.unwrap();
        store
            .set_wonder_skill("missing", false, "t3")
            .await
            .unwrap();
        assert_eq!(store.enabled_wonder_skills().await.unwrap(), ["a"]);
    }

    // Contract: only the owner's own message from a paired device counts as
    // written on the phone; this Mac, an agent writing with the phone's device
    // and an automation run do not.
    #[tokio::test]
    async fn only_paired_device_messages_come_from_the_phone() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("phone", "iPhone", "{}", "now")
            .await
            .unwrap();
        store.ensure_local_desktop("now").await.unwrap();
        let insert = |device: &'static str, client: &'static str| {
            let store = store.clone();
            async move {
                let MessageInsert::Inserted(message) = store
                    .insert_dispatch_message(device, client, "hi", "h", "default", &[], "now", true)
                    .await
                    .unwrap()
                else {
                    panic!("new message")
                };
                message.id
            }
        };
        let phone = insert("phone", "c1").await;
        let desktop = insert("wonder-desktop", "c2").await;
        let automation = insert("phone", "automation:run-1").await;
        let MessageInsert::Inserted(agent) = store
            .insert_agent_message(
                "phone",
                "c4",
                "from an agent",
                "default",
                "now",
                &NewMessageSource {
                    kind: "thread",
                    conversation: Some("default"),
                    title: "Other",
                    wake_parent: None,
                    wake_children: &[],
                },
            )
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        assert!(store.message_from_paired_device(&phone).await.unwrap());
        for other in [&desktop, &automation, &agent.id] {
            assert!(!store.message_from_paired_device(other).await.unwrap());
        }
    }
}
