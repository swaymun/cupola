//! One rule for how a model is named, whichever provider listed it. Cursor
//! names most models by their slug ("gpt-5.6-sol", "claude-opus-5-5") and
//! Codex hyphenates GPT names ("GPT-6.1-Sol"); every client and timeline row
//! reads "GPT-6.1 Sol" and "Claude Opus 5.5" instead. A curated name such as
//! "Auto" or "Opus 5.5" is kept.

use crate::AgentFamily;

const ACRONYMS: [&str; 2] = ["gpt", "glm"];

/// The name to show for `id`, given the name its provider listed. A missing
/// name, or one that only repeats the id, is built from the id's slug: the
/// part before Cursor's "[option=…]" suffix.
pub(crate) fn display_name(family: AgentFamily, id: &str, listed: Option<&str>) -> String {
    let native = family
        .provider()
        .model_id_namespace
        .and_then(|namespace| id.strip_prefix(namespace))
        .unwrap_or(id);
    let slug = native.split('[').next().unwrap_or(native);
    let curated = listed
        .map(str::trim)
        .filter(|name| !name.is_empty() && ![slug, native, id].contains(name));
    let name = match curated {
        Some(name) => name.to_owned(),
        None if is_slug(slug) => slug_words(slug),
        None => return native.to_owned(),
    };
    gpt_spelling(&name)
}

fn is_slug(value: &str) -> bool {
    value.starts_with(|c: char| c.is_ascii_alphabetic())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// "1", "5.6"; Cursor also writes 5.3 as "5p3".
fn version(token: &str) -> Option<String> {
    let numeric = |t: &str| {
        !t.is_empty()
            && t.split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    if numeric(token) {
        return Some(token.to_owned());
    }
    let (major, minor) = token.split_once('p')?;
    (numeric(major) && numeric(minor) && !major.contains('.') && !minor.contains('.'))
        .then(|| format!("{major}.{minor}"))
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// "claude-opus-4-8" is Claude Opus 4.8, "gpt-5.6-sol" GPT-5.6 Sol and
/// "glm-5p3" GLM 5.3.
fn slug_words(slug: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for token in slug.split('-').filter(|t| !t.is_empty()) {
        let Some(v) = version(token) else {
            let acronym = ACRONYMS.contains(&token.to_ascii_lowercase().as_str());
            words.push(if acronym {
                token.to_ascii_uppercase()
            } else {
                capitalized(token)
            });
            continue;
        };
        match words.last_mut() {
            // "opus-4-8": a whole-number version takes the next as its minor.
            Some(last) if !last.contains('.') && version(last).is_some() => {
                last.push('.');
                last.push_str(&v);
            }
            Some(last) if last == "GPT" => {
                last.push('-');
                last.push_str(&v);
            }
            _ => words.push(v),
        }
    }
    words.join(" ")
}

/// GPT names keep one hyphen, before the version: "GPT-6.1-Sol" and
/// "gpt-5.1-codex-mini" read "GPT-6.1 Sol" and "GPT-5.1 Codex Mini".
fn gpt_spelling(name: &str) -> String {
    let Some(rest) = name
        .get(..4)
        .filter(|head| head.eq_ignore_ascii_case("gpt-"))
        .map(|_| &name[4..])
    else {
        return name.to_owned();
    };
    let mut tokens = rest.split(['-', ' ']).filter(|t| !t.is_empty());
    let Some(model) = tokens
        .next()
        .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
    else {
        return name.to_owned();
    };
    std::iter::once(format!("GPT-{model}"))
        .chain(tokens.map(capitalized))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_and_cursor_gpt_names_read_the_same() {
        let codex = |listed| display_name(AgentFamily::Codex, "gpt-6.1-sol", Some(listed));
        assert_eq!(codex("GPT-6.1-Sol"), "GPT-6.1 Sol");
        assert_eq!(codex("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(
            display_name(
                AgentFamily::Codex,
                "gpt-5.1-codex-mini",
                Some("GPT-5.1-Codex-Mini")
            ),
            "GPT-5.1 Codex Mini"
        );
        assert_eq!(
            display_name(
                AgentFamily::Cursor,
                "cursor:gpt-5.6-sol[reasoning=medium]",
                Some("gpt-5.6-sol")
            ),
            "GPT-5.6 Sol"
        );
        assert_eq!(
            display_name(AgentFamily::Codex, "gpt-4o", Some("GPT-4o-mini")),
            "GPT-4o Mini"
        );
        // Names that are not "GPT-<version>" keep their spelling.
        assert_eq!(
            display_name(AgentFamily::Codex, "gpt-reserve", Some("GPT-Reserve")),
            "GPT-Reserve"
        );
        assert_eq!(
            display_name(
                AgentFamily::Codex,
                "codex-auto-review",
                Some("Codex Auto Review")
            ),
            "Codex Auto Review"
        );
    }

    #[test]
    fn cursor_slugs_become_names_and_curated_names_stay() {
        let rows = [
            ("default[]", Some("Auto"), "Auto"),
            (
                "claude-opus-5-5[context=300k,effort=medium,fast=false]",
                Some("claude-opus-5-5"),
                "Claude Opus 5.5",
            ),
            (
                "claude-sonnet-4[thinking=false,context=200k]",
                Some("claude-sonnet-4"),
                "Claude Sonnet 4",
            ),
            (
                "gpt-5.6-sol[context=272k,reasoning=medium,fast=false]",
                Some("gpt-5.6-sol"),
                "GPT-5.6 Sol",
            ),
            ("gpt-5-mini[]", Some("gpt-5-mini"), "GPT-5 Mini"),
            (
                "gpt-5.3-codex[reasoning=medium,fast=false]",
                Some("gpt-5.3-codex"),
                "GPT-5.3 Codex",
            ),
            (
                "glm-5p3-flash[reasoning=high]",
                Some("glm-5p3-flash"),
                "GLM 5.3 Flash",
            ),
            ("kimi-k2.7-code[]", Some("kimi-k2.7-code"), "Kimi K2.7 Code"),
            ("composer-2.5[fast=true]", None, "Composer 2.5"),
            ("gpt-5.6-luna[x=1]", Some("GPT-5.6 Luna"), "GPT-5.6 Luna"),
            ("grok-4.7[fast=true]", Some("grok-4.7"), "Grok 4.7"),
            ("x_y", Some("x_y"), "x_y"),
        ];
        for (native, listed, expected) in rows {
            let id = format!("cursor:{native}");
            assert_eq!(
                display_name(AgentFamily::Cursor, &id, listed),
                expected,
                "{native}"
            );
        }
    }

    #[test]
    fn other_providers_keep_their_names() {
        assert_eq!(
            display_name(AgentFamily::Claude, "claude:opus", Some("Opus 5.5")),
            "Opus 5.5"
        );
        assert_eq!(
            display_name(
                AgentFamily::OpenCode,
                "opencode:opencode/big-pickle",
                Some("Big Pickle")
            ),
            "Big Pickle"
        );
        assert_eq!(
            display_name(AgentFamily::OpenCode, "opencode:local/qwen", None),
            "local/qwen"
        );
    }
}
