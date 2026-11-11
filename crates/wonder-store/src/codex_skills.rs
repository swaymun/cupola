use super::*;
use serde::{Deserialize, Serialize};

/// The selected native item and the Project scope that advertised it. No skill
/// contents are copied into Wonder; Codex reads its own selected path.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexSkillSelection {
    pub name: String,
    pub path: String,
    pub project_id: String,
    pub folder_id: String,
    pub roots_revision: i64,
}

/// Only selections made in the picker become typed inputs. Arbitrary dollar
/// text is never resolved into a skill. Editing away the exact token removes
/// its selection, including in the durable queue.
pub fn skill_token_present(body: &str, name: &str) -> bool {
    let token = format!("${name}");
    body.match_indices(&token).any(|(index, _)| {
        let before = body[..index].chars().next_back();
        let after = body[index + token.len()..].chars().next();
        let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '$');
        before.is_none_or(|c| !word(c)) && after.is_none_or(|c| !word(c))
    })
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_message_with_skills(
        &self,
        device: &str,
        client: &str,
        body: &str,
        hash: &str,
        conversation: &str,
        attachments: &[String],
        now: &str,
        guide_turn: Option<&str>,
        skills: &[CodexSkillSelection],
    ) -> Result<MessageInsert, sqlx::Error> {
        self.insert_work_message(
            device,
            client,
            body,
            hash,
            conversation,
            attachments,
            now,
            guide_turn.is_none(),
            guide_turn,
            None,
            skills,
        )
        .await
    }

    /// Immutable skill identity used only to compare request replays. The
    /// existing body and delivery-kind replay checks still apply.
    pub async fn original_message_codex_skills(
        &self,
        id: &str,
    ) -> Result<Vec<CodexSkillSelection>, sqlx::Error> {
        let value: String =
            sqlx::query_scalar("SELECT original_codex_skills_json FROM messages WHERE id=?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        serde_json::from_str(&value).map_err(|e| sqlx::Error::Protocol(e.to_string()))
    }

    pub async fn message_codex_skills(
        &self,
        id: &str,
    ) -> Result<Vec<CodexSkillSelection>, sqlx::Error> {
        let value: String = sqlx::query_scalar("SELECT codex_skills_json FROM messages WHERE id=?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        serde_json::from_str(&value).map_err(|e| sqlx::Error::Protocol(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn typed_skills_survive_reopen_queue_guide_and_conflict_on_changed_selection() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("store.sqlite").display()
        );
        let store = Store::connect(&url).await.unwrap();
        store
            .upsert_owner_device("phone", "Phone", "{}", "now")
            .await
            .unwrap();
        let skill = CodexSkillSelection {
            name: "review".into(),
            path: "/native/review/SKILL.md".into(),
            project_id: "project".into(),
            folder_id: "folder".into(),
            roots_revision: 1,
        };
        let skills = vec![skill.clone()];
        let MessageInsert::Inserted(message) = store
            .insert_message_with_skills(
                "phone",
                "client",
                "$review this",
                "hash",
                "chat",
                &[],
                "now",
                None,
                &skills,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            store
                .insert_message_with_skills(
                    "phone",
                    "client",
                    "$review this",
                    "hash",
                    "chat",
                    &[],
                    "now",
                    None,
                    &skills
                )
                .await
                .unwrap(),
            MessageInsert::Existing(_)
        ));
        let mut changed = skill.clone();
        changed.path = "/other/review/SKILL.md".into();
        assert!(matches!(
            store
                .insert_message_with_skills(
                    "phone",
                    "client",
                    "$review this",
                    "hash",
                    "chat",
                    &[],
                    "now",
                    None,
                    &[changed]
                )
                .await
                .unwrap(),
            MessageInsert::Conflict
        ));
        drop(store);
        let store = Store::connect(&url).await.unwrap();
        assert_eq!(
            store.message_codex_skills(&message.id).await.unwrap(),
            skills
        );
        assert!(store
            .edit_pending("chat", &message.id, 1, Some(("Keep $review", "edit-hash")))
            .await
            .unwrap());
        assert_eq!(
            store.pending_queue("chat").await.unwrap()[0].codex_skills,
            skills
        );
        let MessageInsert::Inserted(active) = store
            .insert_message(
                "phone",
                "active",
                "running",
                "active-hash",
                "chat",
                "earlier",
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        store
            .update_message_delivery(&active.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        assert!(store
            .queue_to_guide("chat", &message.id, 2, "turn")
            .await
            .unwrap());
        assert_eq!(
            store.message_codex_skills(&message.id).await.unwrap(),
            skills
        );
        assert!(store.claim_guide(&message.id, "thread").await.unwrap());
        assert!(store.guide_to_queue(&message.id).await.unwrap());
        let queued = store.pending_queue("chat").await.unwrap();
        assert!(store
            .edit_pending(
                "chat",
                &message.id,
                queued[0].revision,
                Some(("$reviewer is different", "removed-hash"))
            )
            .await
            .unwrap());
        assert!(store
            .message_codex_skills(&message.id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .original_message_codex_skills(&message.id)
                .await
                .unwrap(),
            skills
        );
        assert!(!store
            .edit_pending("chat", &message.id, 1, Some(("$review", "stale")))
            .await
            .unwrap());
    }
    #[test]
    fn selected_token_has_word_boundaries() {
        for text in ["$review", "Use $review.", "($review)", "$review\nnext"] {
            assert!(skill_token_present(text, "review"), "{text}");
        }
        for text in [
            "$reviewer",
            "$review-extra",
            "x$review",
            "$$review",
            "$review_2",
            "$reviewé",
        ] {
            assert!(!skill_token_present(text, "review"), "{text}");
        }
    }
}
