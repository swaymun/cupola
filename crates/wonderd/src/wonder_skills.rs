//! Wonder's own skill pack: short SKILL.md instructions about what changes when
//! the owner works from the phone (links that open there, readable replies).
//!
//! Wonder is a client and does not change an agent's experience by default.
//! Every skill is off until the owner turns it on in Settings; while all are
//! off nothing here sends anything. When one is on, Wonder's sessions (never
//! the owner's global `~/.claude` or `~/.codex`) can discover the enabled
//! skills from a Wonder-owned folder, and only their descriptions sit in the
//! agent's context until a turn needs one. Turns the owner wrote on a paired
//! phone also carry a one-line phone signal as per-turn context, never in
//! the owner's message. Nothing is added to system prompts or developer
//! instructions.
use crate::{AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path as FsPath, PathBuf};
use wonder_app_server::RpcClient;
use wonder_store::{AgentFamily, StoredMessage, WonderSkillDelivery};

pub(crate) struct WonderSkill {
    /// The SKILL.md `name` and the skill's folder.
    pub id: &'static str,
    /// What Settings calls it.
    pub title: &'static str,
    /// What it changes, in the owner's words.
    pub summary: &'static str,
    pub body: &'static str,
}

/// The pack, as shipped in `skills/` at the repository root.
pub(crate) const SKILLS: [WonderSkill; 3] = [
    WonderSkill {
        id: "wonder-share-to-phone",
        title: "Share pages to your phone",
        summary: "Instead of a localhost link, the agent points you to Preview, which opens the page on your phone. If you ask for a link, it shares one on your Tailscale network (never the public internet) and removes it when you're done.",
        body: include_str!("../../../skills/wonder-share-to-phone/SKILL.md"),
    },
    WonderSkill {
        id: "wonder-phone-replies",
        title: "Phone-friendly replies",
        summary: "Short replies that read well on a small screen and show results, such as a screenshot, instead of asking you to open files on your computer.",
        body: include_str!("../../../skills/wonder-phone-replies/SKILL.md"),
    },
    WonderSkill {
        id: "wonder-phone-questions",
        title: "Questions you can tap to answer",
        summary: "When the agent needs your decision, it asks with choices you can tap in the chat or on the notification, and makes easily undone choices itself instead of asking.",
        body: include_str!("../../../skills/wonder-phone-questions/SKILL.md"),
    },
];

/// The phone signal, sent beside the owner's message: Codex receives it as
/// native turn context, Claude as the bridge's `wonderContext`, which Claude
/// Code gets from a UserPromptSubmit hook. Earlier Claude turns carried it as
/// a line of their input, which the history views never show as something
/// the owner wrote.
pub(crate) const PHONE_CONTEXT: &str =
    "The user sent this message from the Cupola app on their phone, away from this computer's screen.";
const PHONE_CONTEXT_KEY: &str = "wonder_phone";
/// The Claude plugin's name, which prefixes its skills (`wonder:<id>`).
const PLUGIN_NAME: &str = "wonder";

pub(crate) fn is_phone_context(text: &str) -> bool {
    text.trim() == PHONE_CONTEXT
}

/// Wonder-owned folder holding the plugin manifest and `skills/<id>/SKILL.md`
/// for exactly the enabled skills.
pub(crate) fn folder(state: &AppState) -> PathBuf {
    state.data_dir.join("agent-skills")
}

fn skill(id: &str) -> Option<&'static WonderSkill> {
    SKILLS.iter().find(|skill| skill.id == id)
}

fn enabled_skills(ids: &[String]) -> Vec<&'static WonderSkill> {
    SKILLS
        .iter()
        .filter(|skill| ids.iter().any(|id| id == skill.id))
        .collect()
}

/// Writes the manifest and the enabled skills, leaving unchanged files alone,
/// and removes every other skill folder so a harness lists only what is on.
async fn install(folder: PathBuf, enabled: Vec<&'static WonderSkill>) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || install_blocking(&folder, &enabled))
        .await
        .map_err(std::io::Error::other)?
}

fn install_blocking(folder: &FsPath, enabled: &[&WonderSkill]) -> std::io::Result<()> {
    fn write(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
        if std::fs::read(path).ok().as_deref() == Some(bytes) {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap_or(path))?;
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(temporary, path)
    }
    let manifest = json!({
        "name": PLUGIN_NAME,
        "description": "Cupola's skills for working from your phone.",
    });
    write(
        &folder.join(".claude-plugin/plugin.json"),
        serde_json::to_string_pretty(&manifest)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    let skills = folder.join("skills");
    std::fs::create_dir_all(&skills)?;
    for entry in std::fs::read_dir(&skills)? {
        let entry = entry?;
        let name = entry.file_name();
        if !enabled.iter().any(|skill| name == skill.id) {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
    }
    for skill in enabled {
        write(
            &skills.join(skill.id).join("SKILL.md"),
            skill.body.as_bytes(),
        )?;
    }
    Ok(())
}

/// What one turn carries of Wonder's skills.
pub(crate) struct TurnSkills {
    delivery: Option<WonderSkillDelivery>,
    folder: PathBuf,
    enabled: Vec<&'static WonderSkill>,
    /// The owner wrote this turn on a paired phone, and a skill is on.
    phone: bool,
}

impl TurnSkills {
    pub(crate) async fn for_message(
        state: &AppState,
        family: AgentFamily,
        message: &StoredMessage,
    ) -> Self {
        let mut turn = Self {
            delivery: family.provider().skill_delivery,
            folder: folder(state),
            enabled: Vec::new(),
            phone: false,
        };
        if turn.delivery.is_none() {
            return turn;
        }
        // Unreadable settings count as off: nothing is sent.
        let ids = match state.store.enabled_wonder_skills().await {
            Ok(ids) => ids,
            Err(error) => {
                let _ = state.logger.record(
                    "warn",
                    "wonder_skills_unreadable",
                    json!({"error": error.to_string()}),
                );
                return turn;
            }
        };
        let enabled = enabled_skills(&ids);
        if enabled.is_empty() {
            return turn;
        }
        // An update may have changed a skill's text, or the folder was removed.
        if let Err(error) = install(turn.folder.clone(), enabled.clone()).await {
            let _ = state.logger.record(
                "warn",
                "wonder_skills_install_failed",
                json!({"error": error.to_string()}),
            );
            return turn;
        }
        turn.enabled = enabled;
        turn.phone = state
            .store
            .message_from_paired_device(&message.id)
            .await
            .unwrap_or(false);
        turn
    }

    /// Codex App Server keeps extra skill roots in the runtime's memory. Each
    /// runtime starts without them, so they are set once per runtime when a
    /// skill is on, and cleared there once all are off. Nothing is sent to a
    /// runtime that never had one.
    pub(crate) async fn prepare_runtime(&self, state: &AppState, rpc: &RpcClient) {
        if self.delivery != Some(WonderSkillDelivery::AppServerRoot) {
            return;
        }
        let generation = rpc.health().id().to_owned();
        let roots: Vec<String> = if self.enabled.is_empty() {
            Vec::new()
        } else {
            vec![self.folder.join("skills").to_string_lossy().into_owned()]
        };
        let current = state.projects.wonder_skill_roots();
        let current = current.filter(|(applied, _)| *applied == generation);
        match &current {
            Some((_, applied)) if *applied == roots => return,
            None if roots.is_empty() => return,
            _ => {}
        }
        let applied = rpc
            .request("skills/extraRoots/set", json!({ "extraRoots": roots }))
            .await
            .map_err(|error| error.to_string())
            .and_then(|response| match response.error {
                Some(error) => Err(error.message),
                None => Ok(()),
            });
        match applied {
            Ok(()) => state
                .projects
                .set_wonder_skill_roots(Some((generation, roots))),
            Err(error) => {
                let _ = state.logger.record(
                    "warn",
                    "wonder_skills_root_failed",
                    json!({"error": error}),
                );
            }
        }
    }

    /// Adds the session's skills and the phone context to `turn/start`.
    pub(crate) fn apply(&self, params: &mut Value) {
        match self.delivery {
            Some(WonderSkillDelivery::AppServerRoot) if self.phone => {
                params["additionalContext"] = json!({
                    PHONE_CONTEXT_KEY: {"kind": "application", "value": PHONE_CONTEXT}
                });
            }
            Some(WonderSkillDelivery::BridgePlugin) if !self.enabled.is_empty() => {
                params["wonderSkills"] = json!({
                    "plugin": self.folder,
                    "skills": self.enabled.iter()
                        .map(|skill| format!("{PLUGIN_NAME}:{}", skill.id))
                        .collect::<Vec<_>>(),
                });
                if self.phone {
                    params["wonderContext"] = json!([PHONE_CONTEXT]);
                }
            }
            _ => {}
        }
    }
}

/// Every shipped skill with whether it is on, or the refusal when the
/// settings cannot be read.
async fn listing(state: &AppState) -> Response {
    let Ok(ids) = state.store.enabled_wonder_skills().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Cupola couldn't read these settings. Try again.",
        )
            .into_response();
    };
    Json(json!({
        "skills": SKILLS.iter().map(|skill| json!({
            "id": skill.id, "title": skill.title, "summary": skill.summary,
            "enabled": ids.iter().any(|id| id == skill.id),
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetWonderSkill {
    enabled: bool,
}

pub(crate) async fn set(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<SetWonderSkill>,
) -> Response {
    let Some(skill) = skill(&id) else {
        return (StatusCode::NOT_FOUND, "This skill is not part of Cupola.").into_response();
    };
    let unavailable = |message: &'static str| (StatusCode::SERVICE_UNAVAILABLE, message);
    let Ok(mut ids) = state.store.enabled_wonder_skills().await else {
        return unavailable("Cupola couldn't read these settings. Try again.").into_response();
    };
    ids.retain(|enabled| enabled != skill.id);
    if request.enabled {
        ids.push(skill.id.to_owned());
    }
    if install(folder(&state), enabled_skills(&ids)).await.is_err() {
        return unavailable("Cupola couldn't prepare this skill on your computer. Try again.")
            .into_response();
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if state
        .store
        .set_wonder_skill(skill.id, request.enabled, &now)
        .await
        .is_err()
    {
        return unavailable("The setting could not be saved. Try again.").into_response();
    }
    listing(&state).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn frontmatter(body: &str) -> (String, String) {
        let rest = body
            .strip_prefix("---\n")
            .expect("frontmatter opens the file");
        let (head, _) = rest.split_once("\n---\n").expect("frontmatter closes");
        let field = |key: &str| {
            head.lines()
                .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                .unwrap_or_default()
                .to_owned()
        };
        (field("name"), field("description"))
    }

    // Contract: every shipped skill is a valid SKILL.md for both harnesses:
    // its `name` is its folder, and its description (the only part kept in
    // context) is present, short and free of angle brackets.
    #[test]
    fn shipped_skills_have_valid_frontmatter() {
        for skill in &SKILLS {
            let (name, description) = frontmatter(skill.body);
            assert_eq!(name, skill.id);
            assert!(skill
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
            assert!(
                !description.is_empty() && description.len() <= 1024,
                "{name}"
            );
            assert!(!description.contains(['<', '>']), "{name}");
            assert!(description.to_lowercase().contains("phone"), "{name}");
        }
    }

    // Contract: Settings reads every skill as off until the owner turns one
    // on; a switch persists on the host and prepares the folder, and an
    // unknown skill or a malformed body is refused.
    #[tokio::test]
    async fn settings_start_off_and_switches_persist() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        let listed = call(&state, "GET", "/api/v1/settings/wonder-skills", Value::Null).await;
        assert_eq!(listed.0, StatusCode::OK, "{}", listed.1);
        crate::tests::validate_http_contract("wonderSkillSettings", &listed.1);
        let skills = listed.1["skills"].as_array().unwrap();
        assert_eq!(skills.len(), SKILLS.len());
        assert!(skills.iter().all(|skill| skill["enabled"] == false));
        // Each skill, on its own: only it turns on and only its folder is
        // installed; turning it off removes it again.
        let installed = |id: &str| {
            dir.path()
                .join("agent-skills/skills")
                .join(id)
                .join("SKILL.md")
                .exists()
        };
        for (index, skill) in SKILLS.iter().enumerate() {
            let path = format!("/api/v1/settings/wonder-skills/{}", skill.id);
            let on = call(&state, "PUT", &path, json!({"enabled": true})).await;
            assert_eq!(on.0, StatusCode::OK, "{}", on.1);
            crate::tests::validate_http_contract("wonderSkillSettings", &on.1);
            for (other, listed) in on.1["skills"].as_array().unwrap().iter().enumerate() {
                assert_eq!(listed["enabled"], other == index, "{}", skill.id);
            }
            assert_eq!(
                state.store.enabled_wonder_skills().await.unwrap(),
                [skill.id]
            );
            for other in &SKILLS {
                assert_eq!(installed(other.id), other.id == skill.id, "{}", skill.id);
            }
            let reread = call(&state, "GET", "/api/v1/settings/wonder-skills", Value::Null).await;
            assert_eq!(reread.1, on.1);
            let off = call(&state, "PUT", &path, json!({"enabled": false})).await;
            assert_eq!(off.0, StatusCode::OK);
            assert!(state
                .store
                .enabled_wonder_skills()
                .await
                .unwrap()
                .is_empty());
            assert!(!installed(skill.id), "{}", skill.id);
        }
        let path = format!("/api/v1/settings/wonder-skills/{}", SKILLS[1].id);
        let unknown = call(
            &state,
            "PUT",
            "/api/v1/settings/wonder-skills/deploy",
            json!({"enabled": true}),
        )
        .await;
        assert_eq!(unknown.0, StatusCode::NOT_FOUND);
        let malformed = call(&state, "PUT", &path, json!({"enabled": "yes"})).await;
        assert!(malformed.0.is_client_error(), "{}", malformed.0);
        assert!(state
            .store
            .enabled_wonder_skills()
            .await
            .unwrap()
            .is_empty());
    }

    // Contract: a skill an update removed (Blender to GLB) is neither listed
    // nor installed, even when the owner had turned it on; the next switch
    // clears its leftover folder.
    #[tokio::test]
    async fn a_removed_skill_stays_off_after_an_update() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        let removed = "wonder-blender-glb";
        state
            .store
            .set_wonder_skill(removed, true, "2026-10-21T00:00:00.000Z")
            .await
            .unwrap();
        let leftover = dir.path().join("agent-skills/skills").join(removed);
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("SKILL.md"), "---\nname: old\n---\n").unwrap();
        let listed = call(&state, "GET", "/api/v1/settings/wonder-skills", Value::Null).await;
        let ids: Vec<_> = listed.1["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|skill| skill["id"].as_str().unwrap().to_owned())
            .collect();
        assert!(!ids.iter().any(|id| id == removed), "{ids:?}");
        let refused = call(
            &state,
            "PUT",
            &format!("/api/v1/settings/wonder-skills/{removed}"),
            json!({"enabled": true}),
        )
        .await;
        assert_eq!(refused.0, StatusCode::NOT_FOUND);
        let path = format!("/api/v1/settings/wonder-skills/{}", SKILLS[1].id);
        let on = call(&state, "PUT", &path, json!({"enabled": true})).await;
        assert_eq!(on.0, StatusCode::OK, "{}", on.1);
        assert!(!leftover.exists());
    }

    // Contract: the folder holds exactly the enabled skills, so a harness
    // can list nothing that is off; rewriting is idempotent.
    #[test]
    fn install_keeps_only_enabled_skills() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent-skills");
        install_blocking(&root, &SKILLS.iter().collect::<Vec<_>>()).unwrap();
        assert_eq!(
            std::fs::read_dir(root.join("skills")).unwrap().count(),
            SKILLS.len()
        );
        install_blocking(&root, &[&SKILLS[1]]).unwrap();
        install_blocking(&root, &[&SKILLS[1]]).unwrap();
        let listed: Vec<_> = std::fs::read_dir(root.join("skills"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(listed, [SKILLS[1].id]);
        assert_eq!(
            std::fs::read_to_string(root.join("skills").join(SKILLS[1].id).join("SKILL.md"))
                .unwrap(),
            SKILLS[1].body
        );
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        install_blocking(&root, &[]).unwrap();
        assert_eq!(std::fs::read_dir(root.join("skills")).unwrap().count(), 0);
    }
}
