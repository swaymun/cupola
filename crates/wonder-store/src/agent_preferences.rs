//! The owner's default model, effort and speed for each agent harness.

use crate::{AgentFamily, Store};
use sqlx::Row;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDefaultPreference {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}

impl Store {
    /// No saved computer preference means New chat uses its normal provider.
    pub async fn new_chats_start_in_t3_code(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE((SELECT starts_in_t3_code FROM new_chat_defaults WHERE id=1),0)",
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn set_new_chats_start_in_t3_code(&self, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO new_chat_defaults(id,starts_in_t3_code) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET starts_in_t3_code=excluded.starts_in_t3_code")
            .bind(enabled).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn agent_default_preferences(
        &self,
    ) -> Result<Vec<(AgentFamily, AgentDefaultPreference)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT agent_family,model,effort,service_tier FROM agent_default_preferences",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name: String = row.get("agent_family");
                let family = AgentFamily::ALL.into_iter().find(|f| f.as_str() == name)?;
                Some((
                    family,
                    AgentDefaultPreference {
                        model: row.get("model"),
                        effort: row.get("effort"),
                        service_tier: row.get("service_tier"),
                    },
                ))
            })
            .collect())
    }

    /// A preference with no model removes the row, returning the harness to
    /// Wonder's own rule.
    pub async fn set_agent_default_preference(
        &self,
        family: AgentFamily,
        preference: &AgentDefaultPreference,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        if preference.model.is_none() {
            sqlx::query("DELETE FROM agent_default_preferences WHERE agent_family=?")
                .bind(family.as_str())
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        sqlx::query("INSERT INTO agent_default_preferences(agent_family,model,effort,service_tier,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(agent_family) DO UPDATE SET model=excluded.model,effort=excluded.effort,service_tier=excluded.service_tier,updated_at=excluded.updated_at")
            .bind(family.as_str())
            .bind(&preference.model)
            .bind(&preference.effort)
            .bind(&preference.service_tier)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Models the owner hid, per harness.
    pub async fn hidden_models(&self) -> Result<Vec<(AgentFamily, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT agent_family,model_id FROM hidden_models")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name: String = row.get("agent_family");
                let family = AgentFamily::ALL.into_iter().find(|f| f.as_str() == name)?;
                Some((family, row.get("model_id")))
            })
            .collect())
    }

    /// Replaces the harness's hidden models in one transaction.
    pub async fn set_hidden_models(
        &self,
        family: AgentFamily,
        model_ids: &[String],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM hidden_models WHERE agent_family=?")
            .bind(family.as_str())
            .execute(&mut *tx)
            .await?;
        for id in model_ids {
            sqlx::query("INSERT OR IGNORE INTO hidden_models(agent_family,model_id) VALUES(?,?)")
                .bind(family.as_str())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preferences_persist_per_family_and_clear_with_no_model() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let claude = AgentDefaultPreference {
            model: Some("claude:sonnet".into()),
            effort: Some("high".into()),
            service_tier: None,
        };
        store
            .set_agent_default_preference(AgentFamily::Claude, &claude, "now")
            .await
            .unwrap();
        assert_eq!(
            store.agent_default_preferences().await.unwrap(),
            vec![(AgentFamily::Claude, claude)]
        );
        store
            .set_agent_default_preference(
                AgentFamily::Claude,
                &AgentDefaultPreference::default(),
                "later",
            )
            .await
            .unwrap();
        assert!(store.agent_default_preferences().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hidden_models_are_replaced_per_family() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .set_hidden_models(AgentFamily::Cursor, &["a".into(), "b".into()])
            .await
            .unwrap();
        store
            .set_hidden_models(AgentFamily::Claude, &["c".into()])
            .await
            .unwrap();
        store
            .set_hidden_models(AgentFamily::Cursor, &["b".into()])
            .await
            .unwrap();
        let mut hidden = store.hidden_models().await.unwrap();
        hidden.sort_by(|l, r| l.1.cmp(&r.1));
        assert_eq!(
            hidden,
            vec![
                (AgentFamily::Cursor, "b".to_string()),
                (AgentFamily::Claude, "c".to_string())
            ]
        );
    }
}
