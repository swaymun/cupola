//! The owner's runtime settings for each agent harness on this computer.

use crate::{AgentFamily, Store};
use sqlx::Row;

impl Store {
    /// Saved JSON per harness; a missing row means the harness's defaults.
    pub async fn provider_runtime_configs(
        &self,
    ) -> Result<Vec<(AgentFamily, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT agent_family,config FROM provider_runtime_config")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name: String = row.get("agent_family");
                let family = AgentFamily::ALL.into_iter().find(|f| f.as_str() == name)?;
                Some((family, row.get("config")))
            })
            .collect())
    }

    /// `None` returns the harness to its defaults.
    pub async fn set_provider_runtime_config(
        &self,
        family: AgentFamily,
        config: Option<&str>,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        let Some(config) = config else {
            sqlx::query("DELETE FROM provider_runtime_config WHERE agent_family=?")
                .bind(family.as_str())
                .execute(&self.pool)
                .await?;
            return Ok(());
        };
        sqlx::query("INSERT INTO provider_runtime_config(agent_family,config,updated_at) VALUES(?,?,?) ON CONFLICT(agent_family) DO UPDATE SET config=excluded.config,updated_at=excluded.updated_at")
            .bind(family.as_str())
            .bind(config)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_config_persists_per_family_and_clears() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .set_provider_runtime_config(AgentFamily::Codex, Some("{\"a\":1}"), "now")
            .await
            .unwrap();
        assert_eq!(
            store.provider_runtime_configs().await.unwrap(),
            vec![(AgentFamily::Codex, "{\"a\":1}".to_owned())]
        );
        store
            .set_provider_runtime_config(AgentFamily::Codex, None, "later")
            .await
            .unwrap();
        assert!(store.provider_runtime_configs().await.unwrap().is_empty());
    }
}
