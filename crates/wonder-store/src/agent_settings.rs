//! What Wonder adds to its own sessions of one provider (`agent_family_settings`),
//! as the owner set it from the Mac or the phone. A provider without a row
//! has every default.

use crate::{AgentFamily, Store};
use sqlx::Row;

/// The longest custom instruction, in characters (the table checks it too).
pub const INSTRUCTION_MAX_CHARS: usize = 4000;

/// How much of an agent's work the phone shows in a chat.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChatDetail {
    /// Every activity: commands, edits, tool calls and messages.
    #[default]
    Everything,
    /// Only what the owner and the agent say to each other.
    Conversation,
    /// Everything, plus the provider's thinking summaries where it writes
    /// them (Codex only; Claude's thinking carries no text).
    Thinking,
}

impl ChatDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Everything => "everything",
            Self::Conversation => "conversation",
            Self::Thinking => "thinking",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "everything" => Some(Self::Everything),
            "conversation" => Some(Self::Conversation),
            "thinking" => Some(Self::Thinking),
            _ => None,
        }
    }
}

/// One provider's settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentFamilySettings {
    /// Wonder's thread tools (list, read, send, wait, delegate) are offered
    /// to Wonder's sessions. On by default, as before the setting existed.
    pub thread_tools: bool,
    pub chat_detail: ChatDetail,
    /// The custom instruction is added to Wonder's sessions. Off by default.
    pub instruction_enabled: bool,
    /// Kept while off so turning it on again restores it.
    pub instruction: String,
}

impl Default for AgentFamilySettings {
    fn default() -> Self {
        Self {
            thread_tools: true,
            chat_detail: ChatDetail::Everything,
            instruction_enabled: false,
            instruction: String::new(),
        }
    }
}

impl AgentFamilySettings {
    /// The instruction to add to a turn, only while it is on and not empty.
    pub fn active_instruction(&self) -> Option<&str> {
        let text = self.instruction.trim();
        (self.instruction_enabled && !text.is_empty()).then_some(text)
    }
}

impl Store {
    pub async fn agent_family_settings(
        &self,
        family: AgentFamily,
    ) -> Result<AgentFamilySettings, sqlx::Error> {
        let row = sqlx::query(
            "SELECT thread_tools,chat_detail,instruction_enabled,instruction FROM agent_family_settings WHERE agent_family=?",
        )
        .bind(family.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(AgentFamilySettings::default());
        };
        let detail: String = row.get("chat_detail");
        Ok(AgentFamilySettings {
            thread_tools: row.get::<i64, _>("thread_tools") != 0,
            chat_detail: ChatDetail::parse(&detail).unwrap_or_default(),
            instruction_enabled: row.get::<i64, _>("instruction_enabled") != 0,
            instruction: row.get("instruction"),
        })
    }

    /// The detail one chat overrides its provider with (None follows the
    /// provider). A chat that does not exist has none.
    pub async fn chat_detail_override(
        &self,
        conversation: &str,
    ) -> Result<Option<ChatDetail>, sqlx::Error> {
        let detail: Option<Option<String>> = sqlx::query_scalar(
            "SELECT detail_override FROM project_conversations WHERE conversation_id=?",
        )
        .bind(conversation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(detail
            .flatten()
            .and_then(|detail| ChatDetail::parse(&detail)))
    }

    /// Sets or clears (None) a chat's override; false when the chat does not exist.
    pub async fn set_chat_detail_override(
        &self,
        conversation: &str,
        detail: Option<ChatDetail>,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE project_conversations SET detail_override=? WHERE conversation_id=?",
        )
        .bind(detail.map(ChatDetail::as_str))
        .bind(conversation)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    /// Every chat that overrides its provider, by conversation ID.
    pub async fn chat_detail_overrides(&self) -> Result<Vec<(String, ChatDetail)>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT conversation_id,detail_override FROM project_conversations \
             WHERE detail_override IS NOT NULL ORDER BY conversation_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, detail)| Some((id, ChatDetail::parse(&detail)?)))
            .collect())
    }

    /// What a chat of `family` shows: its own override, else the provider's.
    pub async fn effective_chat_detail(
        &self,
        conversation: &str,
        family: AgentFamily,
    ) -> Result<ChatDetail, sqlx::Error> {
        if let Some(detail) = self.chat_detail_override(conversation).await? {
            return Ok(detail);
        }
        Ok(self.agent_family_settings(family).await?.chat_detail)
    }

    pub async fn set_agent_thread_tools(
        &self,
        family: AgentFamily,
        enabled: bool,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_family_settings(agent_family,thread_tools,updated_at) VALUES(?,?,?) \
             ON CONFLICT(agent_family) DO UPDATE SET thread_tools=excluded.thread_tools,updated_at=excluded.updated_at",
        )
        .bind(family.as_str())
        .bind(i64::from(enabled))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_agent_chat_detail(
        &self,
        family: AgentFamily,
        detail: ChatDetail,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_family_settings(agent_family,chat_detail,updated_at) VALUES(?,?,?) \
             ON CONFLICT(agent_family) DO UPDATE SET chat_detail=excluded.chat_detail,updated_at=excluded.updated_at",
        )
        .bind(family.as_str())
        .bind(detail.as_str())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Saves the instruction and its switch together. Text over
    /// [`INSTRUCTION_MAX_CHARS`] is refused by the table.
    pub async fn set_agent_instruction(
        &self,
        family: AgentFamily,
        enabled: bool,
        text: &str,
        now: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_family_settings(agent_family,instruction_enabled,instruction,updated_at) VALUES(?,?,?,?) \
             ON CONFLICT(agent_family) DO UPDATE SET instruction_enabled=excluded.instruction_enabled,\
             instruction=excluded.instruction,updated_at=excluded.updated_at",
        )
        .bind(family.as_str())
        .bind(i64::from(enabled))
        .bind(text)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contract: a provider without a row reads every default (thread tools
    // on, everything shown, no instruction); each setting changes only its
    // own column and provider; the table refuses an instruction over the
    // limit and an unknown detail level.
    #[tokio::test]
    async fn settings_default_and_change_one_column_per_provider() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        for family in AgentFamily::ALL {
            assert_eq!(
                store.agent_family_settings(family).await.unwrap(),
                AgentFamilySettings::default()
            );
        }
        assert!(AgentFamilySettings::default()
            .active_instruction()
            .is_none());

        store
            .set_agent_thread_tools(AgentFamily::Codex, false, "1")
            .await
            .unwrap();
        store
            .set_agent_chat_detail(AgentFamily::Codex, ChatDetail::Conversation, "2")
            .await
            .unwrap();
        store
            .set_agent_instruction(AgentFamily::Codex, true, "Answer briefly.", "3")
            .await
            .unwrap();
        let codex = store
            .agent_family_settings(AgentFamily::Codex)
            .await
            .unwrap();
        assert!(!codex.thread_tools);
        assert_eq!(codex.chat_detail, ChatDetail::Conversation);
        store
            .set_agent_chat_detail(AgentFamily::Codex, ChatDetail::Thinking, "2")
            .await
            .unwrap();
        assert_eq!(
            store
                .agent_family_settings(AgentFamily::Codex)
                .await
                .unwrap()
                .chat_detail,
            ChatDetail::Thinking
        );
        assert_eq!(codex.active_instruction(), Some("Answer briefly."));
        assert_eq!(
            store
                .agent_family_settings(AgentFamily::Claude)
                .await
                .unwrap(),
            AgentFamilySettings::default()
        );

        // Off keeps the text but adds nothing.
        store
            .set_agent_instruction(AgentFamily::Codex, false, "Answer briefly.", "4")
            .await
            .unwrap();
        let codex = store
            .agent_family_settings(AgentFamily::Codex)
            .await
            .unwrap();
        assert_eq!(codex.instruction, "Answer briefly.");
        assert!(codex.active_instruction().is_none());
        assert!(!codex.thread_tools, "other columns are unchanged");

        // The limit counts characters, not bytes.
        let longest = "é".repeat(INSTRUCTION_MAX_CHARS);
        store
            .set_agent_instruction(AgentFamily::Claude, true, &longest, "5")
            .await
            .unwrap();
        assert!(store
            .set_agent_instruction(AgentFamily::Claude, true, &format!("{longest}x"), "6")
            .await
            .is_err());
        assert!(sqlx::query(
            "UPDATE agent_family_settings SET chat_detail='summary' WHERE agent_family='codex'"
        )
        .execute(&store.pool)
        .await
        .is_err());
    }
}
