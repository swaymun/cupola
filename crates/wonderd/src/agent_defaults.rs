//! The single owner of agent defaults: model, reasoning effort, service tier
//! and Claude's approval mode. The host resolves them once when a thread is
//! created or attached and stores the result; clients display stored values
//! and read the same choices from the model catalog.

use crate::{AgentFamily, ModelOption, ProviderDefaults, RuntimeCatalog};
use wonder_store::DefaultSpeed;

/// Claude threads start in Auto, which runs inside the sandbox. Codex ignores
/// the column, so it keeps the neutral stored value.
pub(crate) fn claude_approval(family: AgentFamily) -> &'static str {
    family.provider().default_approval
}

/// The model a provider uses when a thread has none: the owner's preferred
/// model while the catalog still offers it, else the first visible model in
/// the provider's own order, which Settings shows as the default.
pub(crate) fn default_model(catalog: &RuntimeCatalog, family: AgentFamily) -> Option<&ModelOption> {
    let preferred = preferred_model(catalog, family);
    let mut visible = catalog
        .models
        .iter()
        .filter(|m| m.agent_family == family && !m.hidden);
    match preferred {
        Some(id) => visible.find(|m| m.id == id),
        None => visible.next(),
    }
}

/// The owner's preferred model id when the catalog still offers it.
fn preferred_model(catalog: &RuntimeCatalog, family: AgentFamily) -> Option<&str> {
    let id = catalog.preferences.get(&family)?.model.as_deref()?;
    catalog
        .models
        .iter()
        .any(|m| m.id == id && m.agent_family == family && !m.hidden)
        .then_some(id)
}

/// The owner's preference for `option` when it is their preferred model.
fn preference_for<'a>(
    catalog: &'a RuntimeCatalog,
    option: &ModelOption,
) -> Option<&'a wonder_store::AgentDefaultPreference> {
    let preference = catalog.preferences.get(&option.agent_family)?;
    (preferred_model(catalog, option.agent_family) == Some(option.id.as_str()))
        .then_some(preference)
}

fn provider_tier(option: &ModelOption) -> Option<&str> {
    match &option.provider_default {
        Some(raw) => raw.service_tier.as_deref(),
        None => option.default_service_tier.as_deref(),
    }
}

/// The preferred effort on the preferred model, else the first effort the
/// model lists, whatever the provider names as its default. A model with no
/// effort choices has none.
pub(crate) fn default_effort(catalog: &RuntimeCatalog, option: &ModelOption) -> Option<String> {
    preference_for(catalog, option)
        .and_then(|p| p.effort.as_deref())
        .filter(|id| option.reasoning_efforts.iter().any(|e| e.id == *id))
        .or_else(|| option.reasoning_efforts.first().map(|e| e.id.as_str()))
        .map(str::to_owned)
}

pub(crate) fn offers_tier(option: &ModelOption, tier: &str) -> bool {
    option.service_tiers.iter().any(|choice| choice.id == tier)
        || provider_tier(option) == Some(tier)
}

/// The preferred speed on the preferred model, else standard for Codex and the
/// catalog default for Claude.
fn default_tier(
    catalog: &RuntimeCatalog,
    family: AgentFamily,
    option: Option<&ModelOption>,
) -> Option<String> {
    if let Some(tier) = option.and_then(|o| preferred_tier(catalog, o)) {
        return Some(tier);
    }
    match family.provider().default_speed {
        DefaultSpeed::Standard => Some("default".to_owned()),
        DefaultSpeed::CatalogDefault => option.and_then(|o| provider_tier(o).map(str::to_owned)),
    }
}

fn preferred_tier(catalog: &RuntimeCatalog, option: &ModelOption) -> Option<String> {
    preference_for(catalog, option)
        .and_then(|p| p.service_tier.clone())
        .filter(|tier| offers_tier(option, tier))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}

/// Fills what is missing and replaces what the model no longer offers. A
/// stored choice wins while the model still offers it, including non-default
/// speeds. A model the catalog does not list keeps the requested values so
/// validation can name the problem.
pub(crate) fn resolve(
    catalog: &RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
) -> Resolved {
    let option = match model {
        Some(id) => catalog
            .models
            .iter()
            .find(|m| m.id == id && m.agent_family == family),
        None => default_model(catalog, family),
    };
    let model = model
        .map(str::to_owned)
        .or_else(|| option.map(|o| o.id.clone()));
    let Some(option) = option else {
        return Resolved {
            model,
            effort: effort.map(str::to_owned),
            service_tier: service_tier
                .map(str::to_owned)
                .or_else(|| default_tier(catalog, family, None)),
        };
    };
    Resolved {
        model,
        effort: effort
            .filter(|id| option.reasoning_efforts.iter().any(|e| e.id == *id))
            .map(str::to_owned)
            .or_else(|| default_effort(catalog, option)),
        service_tier: service_tier
            .filter(|tier| offers_tier(option, tier))
            .map(str::to_owned)
            .or_else(|| default_tier(catalog, family, Some(option))),
    }
}

impl RuntimeCatalog {
    /// Marks each family's default model so clients need not repeat the rule,
    /// and publishes the resolved default effort and speed as the model's own.
    /// Safe to repeat: the provider's own defaults are kept to return to.
    pub fn resolve_defaults(&mut self) {
        for model in &mut self.models {
            model
                .provider_default
                .get_or_insert_with(|| ProviderDefaults {
                    effort: model.default_reasoning_effort.clone(),
                    service_tier: model.default_service_tier.clone(),
                    hidden: model.hidden,
                });
        }
        for model in &mut self.models {
            let provider_hidden = model.provider_default.as_ref().is_some_and(|d| d.hidden);
            let owner_hidden = self
                .owner_hidden
                .get(&model.agent_family)
                .is_some_and(|ids| ids.contains(&model.id));
            model.hidden = provider_hidden || owner_hidden;
        }
        let mut resolved = Vec::new();
        for family in AgentFamily::ALL {
            let chosen = default_model(self, family).map(|m| m.id.clone());
            for (index, model) in self
                .models
                .iter()
                .enumerate()
                .filter(|(_, m)| m.agent_family == family)
            {
                resolved.push((
                    index,
                    chosen.as_deref() == Some(model.id.as_str()),
                    default_effort(self, model),
                    preferred_tier(self, model).or_else(|| provider_tier(model).map(str::to_owned)),
                ));
            }
        }
        for (index, is_default, effort, tier) in resolved {
            let model = &mut self.models[index];
            model.is_default = is_default;
            model.default_reasoning_effort = effort;
            model.default_service_tier = tier;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ChoiceOption;

    fn choice(id: &str) -> ChoiceOption {
        ChoiceOption {
            id: id.into(),
            label: id.into(),
            description: None,
        }
    }

    pub(crate) fn model(
        id: &str,
        family: AgentFamily,
        efforts: &[&str],
        default: Option<&str>,
    ) -> ModelOption {
        ModelOption {
            agent_family: family,
            capabilities: crate::ModelCapabilities::for_family(family),
            id: id.into(),
            display_name: id.into(),
            description: None,
            model_specialty: None,
            hidden: false,
            reasoning_efforts: efforts.iter().map(|e| choice(e)).collect(),
            default_reasoning_effort: default.map(str::to_owned),
            service_tiers: vec![choice("default"), choice("priority")],
            default_service_tier: None,
            is_default: false,
            provider_default: None,
            native_ids: vec![],
        }
    }

    fn catalog() -> RuntimeCatalog {
        RuntimeCatalog {
            models: vec![
                model("claude:haiku", AgentFamily::Claude, &[], None),
                model(
                    "claude:sonnet",
                    AgentFamily::Claude,
                    &["low", "medium", "high"],
                    Some("xhigh"),
                ),
                model(
                    "gpt-a",
                    AgentFamily::Codex,
                    &["low", "medium", "xhigh"],
                    Some("medium"),
                ),
                model("gpt-b", AgentFamily::Codex, &["low", "xhigh"], None),
            ],
            ..Default::default()
        }
    }

    // Contract (owner decision 2026-10-21): with no saved choice a model starts
    // on the first effort it lists, whatever the provider names as its default.
    #[test]
    fn effort_is_the_first_listed_unless_saved() {
        let claude_efforts = ["low", "medium", "high", "xhigh", "max"];
        let codex_efforts = ["low", "medium", "high", "xhigh", "max", "ultra"];
        let mut c = RuntimeCatalog {
            models: vec![
                model("claude:opus", AgentFamily::Claude, &claude_efforts, None),
                model("claude:haiku", AgentFamily::Claude, &[], None),
                model(
                    "gpt-6.1-sol",
                    AgentFamily::Codex,
                    &codex_efforts,
                    Some("low"),
                ),
                model(
                    "gpt-6-astra",
                    AgentFamily::Codex,
                    &codex_efforts,
                    Some("medium"),
                ),
                model(
                    "gpt-x",
                    AgentFamily::Codex,
                    &["medium", "high"],
                    Some("high"),
                ),
            ],
            ..Default::default()
        };
        let effort =
            |c: &RuntimeCatalog, family, id: &str| resolve(c, family, Some(id), None, None).effort;
        assert_eq!(
            effort(&c, AgentFamily::Claude, "claude:opus").as_deref(),
            Some("low")
        );
        assert_eq!(effort(&c, AgentFamily::Claude, "claude:haiku"), None);
        assert_eq!(
            effort(&c, AgentFamily::Codex, "gpt-6.1-sol").as_deref(),
            Some("low")
        );
        assert_eq!(
            effort(&c, AgentFamily::Codex, "gpt-6-astra").as_deref(),
            Some("low")
        );
        assert_eq!(
            effort(&c, AgentFamily::Codex, "gpt-x").as_deref(),
            Some("medium")
        );
        // A new thread with no model takes the first model and its first effort.
        assert_eq!(
            resolve(&c, AgentFamily::Claude, None, None, None)
                .effort
                .as_deref(),
            Some("low")
        );

        // A saved effort on the saved model still wins, and the thread's own
        // stored effort wins over both.
        c.preferences.insert(
            AgentFamily::Claude,
            wonder_store::AgentDefaultPreference {
                model: Some("claude:opus".into()),
                effort: Some("high".into()),
                service_tier: None,
            },
        );
        assert_eq!(
            effort(&c, AgentFamily::Claude, "claude:opus").as_deref(),
            Some("high")
        );
        assert_eq!(
            resolve(
                &c,
                AgentFamily::Claude,
                Some("claude:opus"),
                Some("max"),
                None
            )
            .effort
            .as_deref(),
            Some("max")
        );
        // Settings reads the effort the catalog publishes for each model.
        c.resolve_defaults();
        let published = |id: &str| {
            c.models
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .default_reasoning_effort
                .clone()
        };
        assert_eq!(published("claude:opus").as_deref(), Some("high"));
        assert_eq!(published("gpt-6-astra").as_deref(), Some("low"));
        assert_eq!(published("gpt-x").as_deref(), Some("medium"));
    }

    #[test]
    fn omitted_model_resolves_to_the_family_default_with_its_settings() {
        let c = catalog();
        // The first listed model is the default, Haiku included.
        let claude = resolve(&c, AgentFamily::Claude, None, None, None);
        assert_eq!(claude.model.as_deref(), Some("claude:haiku"));
        assert_eq!(claude.effort, None);
        let codex = resolve(&c, AgentFamily::Codex, None, None, None);
        assert_eq!(codex.model.as_deref(), Some("gpt-a"));
        assert_eq!(codex.effort.as_deref(), Some("low"));
        assert_eq!(codex.service_tier.as_deref(), Some("default"));
        let only_haiku = RuntimeCatalog {
            models: vec![model("claude:haiku", AgentFamily::Claude, &[], None)],
            ..Default::default()
        };
        assert_eq!(
            resolve(&only_haiku, AgentFamily::Claude, None, None, None)
                .model
                .as_deref(),
            Some("claude:haiku")
        );
    }

    #[test]
    fn stored_choices_win_only_while_the_model_offers_them() {
        let c = catalog();
        let kept = resolve(
            &c,
            AgentFamily::Codex,
            Some("gpt-a"),
            Some("xhigh"),
            Some("priority"),
        );
        assert_eq!(kept.effort.as_deref(), Some("xhigh"));
        assert_eq!(kept.service_tier.as_deref(), Some("priority"));
        let replaced = resolve(
            &c,
            AgentFamily::Codex,
            Some("gpt-b"),
            Some("medium"),
            Some("fast"),
        );
        assert_eq!(replaced.effort.as_deref(), Some("low"));
        assert_eq!(replaced.service_tier.as_deref(), Some("default"));
        let none = resolve(
            &c,
            AgentFamily::Claude,
            Some("claude:haiku"),
            Some("high"),
            None,
        );
        assert_eq!(none.effort, None);
    }

    #[test]
    fn create_and_attach_resolve_identical_defaults_per_family() {
        let c = catalog();
        let listed = AgentFamily::ALL
            .into_iter()
            .filter(|family| c.models.iter().any(|m| m.agent_family == *family));
        for family in listed {
            let attach = resolve(&c, family, None, None, None);
            let create = resolve(&c, family, attach.model.as_deref(), None, None);
            assert_eq!(attach, create);
            assert!(attach.model.is_some());
        }
        assert_eq!(claude_approval(AgentFamily::Claude), "auto");
    }

    #[test]
    fn catalog_marks_one_default_per_family() {
        let mut c = catalog();
        c.resolve_defaults();
        let defaults: Vec<_> = c
            .models
            .iter()
            .filter(|m| m.is_default)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(defaults, ["claude:haiku", "gpt-a"]);
        let sonnet = c.models.iter().find(|m| m.id == "claude:sonnet").unwrap();
        assert_eq!(sonnet.default_reasoning_effort.as_deref(), Some("low"));
    }
}
