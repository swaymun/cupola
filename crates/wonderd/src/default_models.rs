//! The owner's default model, effort and speed for each agent harness. The
//! host stores them, the model catalog publishes the result as `isDefault`,
//! `defaultReasoningEffort` and `defaultServiceTier`, and
//! `agent_defaults` falls back to the first model the provider lists when
//! none is set.

use crate::{agent_defaults, AgentFamily, AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wonder_store::AgentDefaultPreference;

/// Settings, New Chat and the composer read the same resolved model choices.
pub(crate) async fn options(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    let catalog = state.runtime_catalog.read().await;
    Json(json!({
        "models": catalog.models.iter().filter(|model| !model.hidden).collect::<Vec<_>>(),
        "allowedApprovalPolicies": [],
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetDefaultModel {
    /// `null` returns the harness to the first model it lists.
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FamilyPreference {
    family: AgentFamily,
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
}

async fn listing(state: &AppState) -> Result<Value, sqlx::Error> {
    let starts_in_t3_code = state.store.new_chats_start_in_t3_code().await?;
    let catalog = state.runtime_catalog.read().await;
    let families: Vec<_> = AgentFamily::ALL
        .into_iter()
        .map(|family| {
            let preference = catalog
                .preferences
                .get(&family)
                .cloned()
                .unwrap_or_default();
            FamilyPreference {
                family,
                model: preference.model,
                effort: preference.effort,
                service_tier: preference.service_tier,
            }
        })
        .collect();
    Ok(json!({ "families": families, "startsInT3Code": starts_in_t3_code }))
}

async fn listing_response(state: &AppState) -> Response {
    match listing(state).await {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Defaults could not be read. Try again.",
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetNewChatDefaults {
    starts_in_t3_code: bool,
}

/// PUT the computer switch independently so model writes cannot reset it.
pub(crate) async fn set_new_chat_defaults(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Json(request): Json<SetNewChatDefaults>,
) -> Response {
    if state
        .store
        .set_new_chats_start_in_t3_code(request.starts_in_t3_code)
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The default could not be saved. Try again.",
        )
            .into_response();
    }
    listing_response(&state).await
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    listing_response(&state).await
}

pub(crate) async fn set(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetDefaultModel>,
) -> Response {
    let preference = AgentDefaultPreference {
        model: request.model,
        effort: request.effort,
        service_tier: request.service_tier,
    };
    {
        let catalog = state.runtime_catalog.read().await;
        if let Err(message) = validate(&catalog, family, &preference) {
            return (StatusCode::UNPROCESSABLE_ENTITY, message).into_response();
        }
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if state
        .store
        .set_agent_default_preference(family, &preference, &now)
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The default could not be saved. Try again.",
        )
            .into_response();
    }
    {
        let mut catalog = state.runtime_catalog.write().await;
        if preference.model.is_some() {
            catalog.preferences.insert(family, preference);
        } else {
            catalog.preferences.remove(&family);
        }
        catalog.resolve_defaults();
    }
    listing_response(&state).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetModelVisibility {
    hidden_models: Vec<String>,
}

/// Every model the provider offers, with whether the owner hid it.
async fn visibility(state: &AppState) -> Value {
    let catalog = state.runtime_catalog.read().await;
    let families: Vec<_> = AgentFamily::ALL
        .into_iter()
        .map(|family| {
            let hidden = catalog.owner_hidden.get(&family);
            let models: Vec<_> = catalog
                .models
                .iter()
                .filter(|m| m.agent_family == family)
                .filter(|m| !m.provider_default.as_ref().is_some_and(|d| d.hidden))
                .map(|m| {
                    json!({
                        "id": m.id,
                        "displayName": m.display_name,
                        "hidden": hidden.is_some_and(|ids| ids.contains(&m.id)),
                    })
                })
                .collect();
            json!({ "family": family, "models": models })
        })
        .collect();
    json!({ "families": families })
}

pub(crate) async fn visibility_list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    Json(visibility(&state).await).into_response()
}

/// Hides exactly these models for the harness, from the composer, New chat
/// and the agent tools. Hiding every model leaves the harness out of New chat.
pub(crate) async fn set_visibility(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetModelVisibility>,
) -> Response {
    let hidden: std::collections::HashSet<String> = request.hidden_models.into_iter().collect();
    let mut ids: Vec<String> = hidden.iter().cloned().collect();
    ids.sort();
    if state.store.set_hidden_models(family, &ids).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The models could not be saved. Try again.",
        )
            .into_response();
    }
    {
        let mut catalog = state.runtime_catalog.write().await;
        if hidden.is_empty() {
            catalog.owner_hidden.remove(&family);
        } else {
            catalog.owner_hidden.insert(family, hidden);
        }
        catalog.resolve_defaults();
    }
    Json(visibility(&state).await).into_response()
}

/// Only what the catalog offers for the harness can be the default.
fn validate(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    preference: &AgentDefaultPreference,
) -> Result<(), &'static str> {
    let Some(model) = preference.model.as_deref() else {
        return if preference.effort.is_some() || preference.service_tier.is_some() {
            Err("Choose a model before changing its settings.")
        } else {
            Ok(())
        };
    };
    let option = catalog
        .models
        .iter()
        .find(|m| m.id == model && m.agent_family == family && !m.hidden)
        .ok_or("This model is not available for this harness.")?;
    if preference
        .effort
        .as_deref()
        .is_some_and(|effort| !option.reasoning_efforts.iter().any(|e| e.id == effort))
    {
        return Err("This model does not support that effort.");
    }
    if preference
        .service_tier
        .as_deref()
        .is_some_and(|tier| !agent_defaults::offers_tier(option, tier))
    {
        return Err("This model does not support that speed.");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent_defaults::tests::model;
    use crate::RuntimeCatalog;

    fn catalog() -> RuntimeCatalog {
        let mut c = RuntimeCatalog {
            models: vec![
                model("claude:haiku", AgentFamily::Claude, &[], None),
                model("claude:sonnet", AgentFamily::Claude, &["low", "high"], None),
                model(
                    "claude:opus",
                    AgentFamily::Claude,
                    &["medium", "xhigh"],
                    None,
                ),
                model(
                    "gpt-a",
                    AgentFamily::Codex,
                    &["low", "medium"],
                    Some("medium"),
                ),
                model("gpt-b", AgentFamily::Codex, &["low", "xhigh"], None),
            ],
            ..Default::default()
        };
        c.resolve_defaults();
        c
    }

    fn prefer(
        c: &mut RuntimeCatalog,
        family: AgentFamily,
        model: &str,
        effort: Option<&str>,
        tier: Option<&str>,
    ) {
        c.preferences.insert(
            family,
            AgentDefaultPreference {
                model: Some(model.into()),
                effort: effort.map(str::to_owned),
                service_tier: tier.map(str::to_owned),
            },
        );
        c.resolve_defaults();
    }

    fn default_of(
        c: &RuntimeCatalog,
        family: AgentFamily,
    ) -> (String, Option<String>, Option<String>) {
        let m = c
            .models
            .iter()
            .find(|m| m.agent_family == family && m.is_default)
            .unwrap();
        (
            m.id.clone(),
            m.default_reasoning_effort.clone(),
            m.default_service_tier.clone(),
        )
    }

    // Contract: the preference decides the default model, effort and speed for
    // its harness, the catalog publishes them, and clearing it, or the model
    // leaving the catalog, returns to the first model the provider lists.
    #[test]
    fn preference_overrides_the_default_and_falls_back_when_unoffered() {
        let mut c = catalog();
        assert_eq!(default_of(&c, AgentFamily::Claude).0, "claude:haiku");
        assert_eq!(default_of(&c, AgentFamily::Codex).0, "gpt-a");

        prefer(
            &mut c,
            AgentFamily::Claude,
            "claude:opus",
            Some("xhigh"),
            None,
        );
        prefer(
            &mut c,
            AgentFamily::Codex,
            "gpt-b",
            Some("xhigh"),
            Some("priority"),
        );
        assert_eq!(default_of(&c, AgentFamily::Claude).0, "claude:opus");
        assert_eq!(
            default_of(&c, AgentFamily::Claude).1.as_deref(),
            Some("xhigh")
        );
        let codex = default_of(&c, AgentFamily::Codex);
        assert_eq!(
            (codex.0.as_str(), codex.1.as_deref(), codex.2.as_deref()),
            ("gpt-b", Some("xhigh"), Some("priority"))
        );
        // A new thread resolves to the same values with no client-side rule.
        let new_thread = agent_defaults::resolve(&c, AgentFamily::Codex, None, None, None);
        assert_eq!(
            (
                new_thread.model.as_deref(),
                new_thread.effort.as_deref(),
                new_thread.service_tier.as_deref()
            ),
            (Some("gpt-b"), Some("xhigh"), Some("priority"))
        );
        // The other harness is untouched.
        assert_eq!(
            agent_defaults::resolve(&c, AgentFamily::Claude, None, None, None)
                .model
                .as_deref(),
            Some("claude:opus")
        );

        // The preferred model disappears from the catalog.
        c.models.retain(|m| m.id != "gpt-b");
        c.resolve_defaults();
        assert_eq!(default_of(&c, AgentFamily::Codex).0, "gpt-a");

        // An effort the model stops offering falls back to the model's own.
        prefer(
            &mut c,
            AgentFamily::Claude,
            "claude:opus",
            Some("xhigh"),
            None,
        );
        c.models
            .iter_mut()
            .find(|m| m.id == "claude:opus")
            .unwrap()
            .reasoning_efforts
            .retain(|e| e.id != "xhigh");
        c.resolve_defaults();
        assert_eq!(
            default_of(&c, AgentFamily::Claude).1.as_deref(),
            Some("medium")
        );

        // Clearing returns to the first listed model and the provider's own tier.
        c.preferences.clear();
        c.resolve_defaults();
        assert_eq!(default_of(&c, AgentFamily::Claude).0, "claude:haiku");
        assert_eq!(default_of(&c, AgentFamily::Codex).2, None);
    }

    #[test]
    fn only_what_the_catalog_offers_can_be_chosen() {
        let c = catalog();
        let pick = |model: Option<&str>, effort: Option<&str>, tier: Option<&str>| {
            validate(
                &c,
                AgentFamily::Codex,
                &AgentDefaultPreference {
                    model: model.map(str::to_owned),
                    effort: effort.map(str::to_owned),
                    service_tier: tier.map(str::to_owned),
                },
            )
        };
        assert!(pick(Some("gpt-b"), Some("xhigh"), Some("priority")).is_ok());
        assert!(pick(None, None, None).is_ok());
        assert!(
            pick(Some("claude:opus"), None, None).is_err(),
            "another harness's model"
        );
        assert!(
            pick(Some("gpt-a"), Some("xhigh"), None).is_err(),
            "unoffered effort"
        );
        assert!(
            pick(Some("gpt-a"), None, Some("turbo")).is_err(),
            "unoffered speed"
        );
        assert!(
            pick(None, Some("low"), None).is_err(),
            "settings without a model"
        );
    }

    // Contract: the HTTP setting persists on the host, updates the catalog the
    // composer reads, and survives a restart; unavailable choices are refused.
    #[tokio::test]
    async fn computer_t3_default_is_off_persists_and_does_not_change_models() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        let path = "/api/v1/settings/default-models";
        let initial = call(&state, "GET", path, Value::Null).await;
        assert_eq!(initial.0, StatusCode::OK);
        assert_eq!(initial.1["startsInT3Code"], false);
        let saved = call(&state, "PUT", path, json!({"startsInT3Code": true})).await;
        assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
        crate::tests::validate_http_contract("defaultModelPreferences", &saved.1);
        assert_eq!(saved.1["startsInT3Code"], true);
        assert_eq!(saved.1["families"], initial.1["families"]);
        let other_computer = wonder_store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        assert!(!other_computer.new_chats_start_in_t3_code().await.unwrap());
        // A separate Store handle is the host-restart persistence boundary.
        let reopened = wonder_store::Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        assert!(reopened.new_chats_start_in_t3_code().await.unwrap());
        let read = call(&state, "GET", path, Value::Null).await;
        assert_eq!(read.0, StatusCode::OK);
        assert_eq!(read.1["startsInT3Code"], true);
        let model_write = call(
            &state,
            "PUT",
            "/api/v1/settings/default-models/codex",
            json!({"model": null}),
        )
        .await;
        assert_eq!(model_write.0, StatusCode::OK);
        assert_eq!(
            model_write.1["startsInT3Code"], true,
            "model changes preserve the computer default"
        );
        let reset = call(&state, "PUT", path, json!({"startsInT3Code": false})).await;
        assert_eq!(reset.0, StatusCode::OK);
        assert_eq!(reset.1["startsInT3Code"], false);
        assert!(!reopened.new_chats_start_in_t3_code().await.unwrap());
        let malformed = call(&state, "PUT", path, json!({"startsInT3Code": "yes"})).await;
        assert_eq!(malformed.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn setting_a_default_persists_and_updates_the_catalog() {
        use crate::test_support::{call, fixture};
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let (_dir, state) = fixture().await;
        let unauthorized = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/agent-options")
                    .header(
                        "origin",
                        crate::current_public_origin(&state).await.unwrap(),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let mut choices = catalog();
        let mut hidden = model("hidden", AgentFamily::Codex, &[], None);
        hidden.hidden = true;
        choices.models.push(hidden);
        *state.runtime_catalog.write().await = choices;
        let options = call(&state, "GET", "/api/v1/agent-options", Value::Null).await;
        assert_eq!(options.0, StatusCode::OK, "{}", options.1);
        crate::tests::validate_http_contract("agentOptions", &options.1);
        let models = options.1["models"].as_array().unwrap();
        assert_eq!(models.len(), 5);
        assert!(models.iter().all(|model| model["hidden"] == false));
        assert!(models.iter().any(|model| model["id"] == "claude:opus"));
        let put = |family: &str, body: Value| {
            let state = state.clone();
            let path = format!("/api/v1/settings/default-models/{family}");
            async move { call(&state, "PUT", &path, body).await }
        };
        let saved = put("claude", json!({"model": "claude:opus", "effort": "xhigh"})).await;
        assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
        crate::tests::validate_http_contract("defaultModelPreferences", &saved.1);
        assert_eq!(saved.1["families"][1]["model"], "claude:opus");
        let options = call(&state, "GET", "/api/v1/agent-options", Value::Null).await;
        let selected = options.1["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "claude:opus")
            .unwrap();
        assert_eq!(selected["isDefault"], true);
        assert_eq!(selected["defaultReasoningEffort"], "xhigh");
        let catalog = state.runtime_catalog.read().await;
        assert_eq!(default_of(&catalog, AgentFamily::Claude).0, "claude:opus");
        drop(catalog);
        let stored = state.store.agent_default_preferences().await.unwrap();
        assert_eq!(stored[0].1.effort.as_deref(), Some("xhigh"));
        let refused = put("codex", json!({"model": "claude:opus"})).await;
        assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY);
        let reset = put("claude", json!({"model": null})).await;
        assert_eq!(reset.0, StatusCode::OK, "{}", reset.1);
        assert!(state
            .store
            .agent_default_preferences()
            .await
            .unwrap()
            .is_empty());
        let catalog = state.runtime_catalog.read().await;
        assert_eq!(default_of(&catalog, AgentFamily::Claude).0, "claude:haiku");
    }

    // Contract: a model the owner hides leaves agent-options and the agent
    // tools' lists, persists, and returns when shown again; hiding every model
    // of a family is allowed and leaves that family with no default.
    #[tokio::test]
    async fn hidden_models_leave_every_list_and_persist() {
        use crate::test_support::{call, fixture};
        let (_dir, state) = fixture().await;
        *state.runtime_catalog.write().await = catalog();
        let put = |family: &str, hidden: Value| {
            let state = state.clone();
            let path = format!("/api/v1/settings/model-visibility/{family}");
            async move { call(&state, "PUT", &path, json!({ "hiddenModels": hidden })).await }
        };
        let saved = put("claude", json!(["claude:haiku"])).await;
        assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
        crate::tests::validate_http_contract("modelVisibility", &saved.1);
        let options = call(&state, "GET", "/api/v1/agent-options", Value::Null).await;
        assert!(options.1["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != "claude:haiku"));
        {
            let catalog = state.runtime_catalog.read().await;
            let listed: Vec<_> =
                crate::thread_tools::listed_models(&catalog, Some(AgentFamily::Claude))
                    .map(|m| m.id.as_str())
                    .collect();
            assert_eq!(listed, ["claude:sonnet", "claude:opus"]);
            assert_eq!(default_of(&catalog, AgentFamily::Claude).0, "claude:sonnet");
            // A thread already on the hidden model keeps sending; choosing it
            // anew (a delegated thread, a model change) is refused.
            let family = AgentFamily::Claude;
            let kept = crate::projects::dispatch_settings(
                &catalog,
                family,
                Some("claude:haiku"),
                None,
                None,
            );
            assert_eq!(kept.unwrap().0, "claude:haiku");
            assert!(crate::projects::execution_settings(
                &catalog,
                family,
                Some("claude:haiku"),
                None,
                None
            )
            .is_err());
            assert!(crate::projects::validate_model(
                &catalog,
                family,
                Some("claude:haiku"),
                None,
                None
            )
            .is_err());
        }
        assert_eq!(
            state.store.hidden_models().await.unwrap(),
            vec![(AgentFamily::Claude, "claude:haiku".to_string())]
        );
        let all = put(
            "claude",
            json!(["claude:haiku", "claude:sonnet", "claude:opus"]),
        )
        .await;
        assert_eq!(all.0, StatusCode::OK, "{}", all.1);
        {
            let catalog = state.runtime_catalog.read().await;
            assert!(
                crate::thread_tools::listed_models(&catalog, Some(AgentFamily::Claude))
                    .next()
                    .is_none()
            );
            assert!(catalog
                .models
                .iter()
                .all(|m| !m.is_default || m.agent_family != AgentFamily::Claude));
        }
        let shown = put("claude", json!([])).await;
        assert_eq!(shown.0, StatusCode::OK);
        let options = call(&state, "GET", "/api/v1/agent-options", Value::Null).await;
        assert!(options.1["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "claude:haiku"));
    }
}
