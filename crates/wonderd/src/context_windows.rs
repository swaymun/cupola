//! The context window a model typically has, for when its provider reports
//! token use without the window (a Claude transcript reply, a T3 Code child)
//! or has not reported anything yet. A provider's own report always wins; a
//! size from here is shown labeled "typical". Sources, check dates and gaps
//! are in docs/context-windows.md; one row here per model or model family.

use crate::AgentFamily;

const fn row(
    family: Option<AgentFamily>,
    pattern: &'static str,
    tokens: u64,
    source: Source,
) -> Row {
    Row {
        family,
        pattern,
        tokens,
        source,
    }
}

/// Where a row's number comes from; the URLs and commands are in the doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Claude Code's model configuration page.
    ClaudeCode,
    /// `codex debug models`: `context_window` times `effective_context_window_percent`.
    CodexModels,
    /// models.dev (also OpenCode's own `~/.cache/opencode/models.json`).
    ModelsDev,
    /// Cursor's models page.
    CursorDocs,
}

impl Source {
    /// How docs/context-windows.md names the source.
    #[cfg(test)]
    fn label(self) -> &'static str {
        match self {
            Source::ClaudeCode => "claude-code-docs",
            Source::CodexModels => "codex-debug-models",
            Source::ModelsDev => "models.dev",
            Source::CursorDocs => "cursor-docs",
        }
    }
}

/// A trailing `*` in `pattern` matches any name that starts with it; otherwise
/// the whole name must match. `family: None` rows apply to every provider.
pub(crate) struct Row {
    pub family: Option<AgentFamily>,
    pub pattern: &'static str,
    pub tokens: u64,
    /// Read by the docs test, which keeps docs/context-windows.md in step.
    #[cfg_attr(not(test), allow(dead_code))]
    pub source: Source,
}

const K: u64 = 1_000;
const M: u64 = 1_000_000;
const GEMINI: u64 = 1_048_576;
/// Codex reports 95% of the 272K window a model lists.
const CODEX: u64 = 258_400;

use Source::{ClaudeCode as CC, CodexModels as CX, CursorDocs as CU, ModelsDev as MD};

pub(crate) const ROWS: &[Row] = &[
    // Claude Code runs the current generation with a 1M window, no suffix.
    row(None, "default", M, CC),
    row(None, "opus", M, CC),
    row(None, "sonnet", M, CC),
    row(None, "haiku", M, CC),
    row(None, "fable", M, CC),
    row(None, "claude-fable-5*", M, CC),
    row(None, "claude-opus-5*", M, CC),
    row(None, "claude-opus-4-7", M, CC),
    row(None, "claude-opus-4-8", M, CC),
    row(None, "claude-sonnet-5*", M, CC),
    row(None, "claude-haiku-5*", M, CC),
    // Opus 4.6 and Sonnet 4.6 reach 1M only through [1m] (handled before this table).
    row(None, "claude-opus-4-6", 200 * K, CC),
    row(None, "claude-sonnet-4-6", 200 * K, CC),
    row(None, "claude-4.6-opus", 200 * K, CC),
    row(None, "claude-4.6-sonnet", 200 * K, CC),
    row(None, "claude-opus-4-5", 200 * K, MD),
    row(None, "claude-sonnet-4-5", 200 * K, MD),
    row(None, "claude-haiku-4-5", 200 * K, MD),
    row(None, "claude-4.5-opus", 200 * K, MD),
    row(None, "claude-4.5-sonnet", 200 * K, MD),
    row(None, "claude-4-sonnet", 200 * K, MD),
    row(None, "claude-sonnet-4", 200 * K, MD),
    row(None, "claude-opus-4-1", 200 * K, MD),
    // Codex reports the window of its rollout; every model it lists has the same one.
    row(Some(AgentFamily::Codex), "gpt-*", CODEX, CX),
    row(Some(AgentFamily::Codex), "gpt-5.3-codex-spark", 121_600, MD),
    // The same models through the API (Cursor, OpenCode).
    row(None, "gpt-6*", 1_050 * K, MD),
    row(None, "gpt-5.6*", 1_050 * K, MD),
    row(None, "gpt-5.5*", 1_050 * K, MD),
    row(None, "gpt-5.4", 1_050 * K, MD),
    row(None, "gpt-5.4-pro", 1_050 * K, MD),
    row(None, "gpt-5.4-mini", 400 * K, MD),
    row(None, "gpt-5.4-nano", 400 * K, MD),
    row(None, "gpt-5.3-codex", 400 * K, MD),
    row(None, "gpt-5.3-codex-spark", 128 * K, MD),
    row(None, "gpt-5.2*", 400 * K, MD),
    row(None, "gpt-5.1*", 400 * K, MD),
    row(None, "gpt-5", 400 * K, MD),
    row(None, "gpt-5-mini", 400 * K, MD),
    row(None, "gpt-5-nano", 400 * K, MD),
    row(None, "gpt-5-pro", 400 * K, MD),
    row(None, "gpt-daybreak-blue-latest", 1_050 * K, MD),
    row(None, "gpt-daybreak-red-latest", 400 * K, MD),
    row(None, "gpt-4.1*", 1_047_576, MD),
    row(None, "gpt-4o*", 128 * K, MD),
    row(None, "o3*", 200 * K, MD),
    row(None, "gemini-3*", GEMINI, MD),
    row(None, "gemini-4*", GEMINI, MD),
    row(None, "gemini-2.5-flash", GEMINI, MD),
    row(None, "gemini-2.5-flash-lite", GEMINI, MD),
    row(None, "gemini-2.5-pro", GEMINI, MD),
    row(None, "gemini-2.5-computer-use-preview-10-2025", 128 * K, MD),
    row(None, "gemini-flash-latest", GEMINI, MD),
    row(None, "gemini-flash-lite-latest", GEMINI, MD),
    row(None, "gemini-pro-latest", GEMINI, MD),
    row(None, "deep-research-preview-04-2026", 131_072, MD),
    row(None, "deep-research-max-preview-04-2026", 131_072, MD),
    row(None, "gemma-4-26b-a4b-it", 262_144, MD),
    row(None, "gemma-4-31b-it", 262_144, MD),
    // Grok 4.7 in Cursor runs 256k unless the 500k variant is picked (the id says so).
    row(Some(AgentFamily::Cursor), "grok-4.7", 256 * K, CU),
    row(None, "grok-4.7", 500 * K, MD),
    row(None, "grok-4.6", 500 * K, MD),
    row(None, "grok-4.5", 500 * K, MD),
    row(None, "kimi-k3", GEMINI, MD),
    row(None, "kimi-k2.7-code", 262_144, MD),
    row(None, "glm-5.2", M, MD),
    row(None, "deepseek-v41-flash-1017", GEMINI, MD),
    row(None, "muse-wonder-1.0", GEMINI, MD),
    row(None, "muse-spark-1.3", GEMINI, MD),
    // OpenCode's free models.
    row(Some(AgentFamily::OpenCode), "big-pickle", 200 * K, MD),
    row(Some(AgentFamily::OpenCode), "beam-free", GEMINI, MD),
    row(
        Some(AgentFamily::OpenCode),
        "deepseek-v4.1-flash-free",
        200 * K,
        MD,
    ),
    row(
        Some(AgentFamily::OpenCode),
        "crouton-alpha-free",
        GEMINI,
        MD,
    ),
    row(Some(AgentFamily::OpenCode), "pulse-alpha-free", GEMINI, MD),
    row(Some(AgentFamily::OpenCode), "flint-40b-free", 262_144, MD),
    row(
        Some(AgentFamily::OpenCode),
        "glm-5.3-flash-free",
        200 * K,
        MD,
    ),
    row(
        Some(AgentFamily::OpenCode),
        "mimo-v2.6-flash-free",
        200 * K,
        MD,
    ),
    row(
        Some(AgentFamily::OpenCode),
        "nemotron-3.5-lightning-free",
        262_144,
        MD,
    ),
    row(
        Some(AgentFamily::OpenCode),
        "nemotron-3.5-super-vl-free",
        GEMINI,
        MD,
    ),
];

/// Effort, speed and thinking variants share their model's window.
const VARIANT_SUFFIXES: [&str; 13] = [
    "fast",
    "flex",
    "ultrafast",
    "pro",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "none",
    "minimal",
    "thinking",
    "extra",
];

/// "272k" and "1m" as written in a model id: "[context=272k,…]" or "[1m]".
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let (digits, unit) = match text.strip_suffix('k') {
        Some(digits) => (digits, K),
        None => (text.strip_suffix('m')?, M),
    };
    let value: f64 = digits.parse().ok()?;
    (value > 0.0).then(|| (value * unit as f64).round() as u64)
}

/// The window the model id itself names: Claude's "[1m]" and Cursor's
/// "[context=272k,…]".
fn declared_window(native: &str) -> Option<u64> {
    let (_, options) = native.split_once('[')?;
    options
        .trim_end_matches(']')
        .split(',')
        .find_map(|option| parse_size(option.strip_prefix("context=").unwrap_or(option)))
}

/// The part of an id that names the model: no provider namespace, option
/// brackets, "provider/" (OpenCode), "cursor-" (Cursor's own Grok slugs) or
/// date stamp ("claude-haiku-4-5-20251001").
fn slug(family: AgentFamily, model: &str) -> String {
    let native = family
        .provider()
        .model_id_namespace
        .and_then(|namespace| model.strip_prefix(namespace))
        .unwrap_or(model);
    let bare = native
        .split('[')
        .next()
        .unwrap_or(native)
        .to_ascii_lowercase();
    let name = bare.rsplit('/').next().unwrap_or(&bare);
    let name = name.strip_prefix("cursor-").unwrap_or(name);
    match name.rsplit_once('-') {
        Some((head, stamp)) if stamp.len() == 8 && stamp.bytes().all(|b| b.is_ascii_digit()) => {
            head.to_owned()
        }
        _ => name.to_owned(),
    }
}

fn find(family: AgentFamily, name: &str) -> Option<&'static Row> {
    ROWS.iter()
        .filter(|row| row.family.is_none_or(|f| f == family))
        .filter(|row| match row.pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => row.pattern == name,
        })
        // A provider's own row beats the general one, then the longer pattern.
        .max_by_key(|row| (row.family.is_some(), row.pattern.len()))
}

/// The row for `model`, dropping effort and speed words from the end until
/// one matches ("gpt-5.5-extra-high-fast" is GPT-5.5).
pub(crate) fn row_for(family: AgentFamily, model: &str) -> Option<&'static Row> {
    let mut name = slug(family, model);
    loop {
        if let Some(row) = find(family, &name) {
            return Some(row);
        }
        let (head, tail) = name.rsplit_once('-')?;
        if !VARIANT_SUFFIXES.contains(&tail) {
            return None;
        }
        name = head.to_owned();
    }
}

/// The window `model` typically has, or `None` for a model nobody has
/// published one for.
pub(crate) fn typical_window(family: AgentFamily, model: &str) -> Option<u64> {
    declared_window(model).or_else(|| row_for(family, model).map(|row| row.tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each CLI listed on 2026-11-06 (scripts/context-window-pickers);
    /// the Claude rows are the bridge's model list and its resolved ids.
    const PICKERS: &str = include_str!("context_windows_pickers.txt");

    /// Listed models that nobody publishes a window for. Each stays honest:
    /// the phone says the size is unknown instead of guessing.
    const UNPUBLISHED: [&str; 7] = [
        "auto",
        "composer-2.5",
        "composer-2.5-fast",
        "chat-latest",
        "gpt-audio-1.5",
        "gpt-5-search-api",
        "gemini-robotics-er-2-preview",
    ];

    /// The doc lists every row with the source it was read from.
    #[test]
    fn the_doc_lists_every_row_with_its_source() {
        let doc = include_str!("../../../docs/context-windows.md");
        for row in ROWS {
            let cell = format!("| `{}` |", row.pattern);
            let tokens = format!("| {} |", row.tokens);
            assert!(
                doc.lines().any(|line| line.contains(&cell)
                    && line.contains(&tokens)
                    && line.contains(row.source.label())),
                "docs/context-windows.md has no `{}` row of {} tokens sourced {}",
                row.pattern,
                row.tokens,
                row.source.label()
            );
        }
    }

    /// Prints the markdown rows for docs/context-windows.md:
    /// `cargo test -p wonderd context_windows::tests::print_doc_rows -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_doc_rows() {
        for row in ROWS {
            println!(
                "| `{}` | {} | {} | {} | 2026-11-06 |",
                row.pattern,
                row.family.map_or("any", |f| f.provider().display_name),
                row.tokens,
                row.source.label()
            );
        }
        println!();
        for line in PICKERS.lines().filter(|l| !l.trim().is_empty()) {
            let (name, id) = line.split_once('\t').unwrap();
            let window =
                typical_window(family(name), id).map_or("unknown".to_owned(), |w| w.to_string());
            println!("{name}\t{id}\t{window}");
        }
    }

    fn family(name: &str) -> AgentFamily {
        match name {
            "claude" => AgentFamily::Claude,
            "codex" => AgentFamily::Codex,
            "cursor" => AgentFamily::Cursor,
            "opencode" => AgentFamily::OpenCode,
            other => panic!("unknown family {other} in context_windows_pickers.txt"),
        }
    }

    #[test]
    fn every_picker_model_has_a_window_or_is_listed_as_unpublished() {
        let mut gaps = Vec::new();
        for line in PICKERS.lines().filter(|l| !l.trim().is_empty()) {
            let (name, id) = line.split_once('\t').expect("family<TAB>id");
            let family = family(name);
            let known = typical_window(family, id).is_some();
            let slug = slug(family, id);
            let unpublished = UNPUBLISHED.contains(&slug.as_str());
            if known == unpublished {
                gaps.push(format!(
                    "{name} {id}: {}",
                    if known {
                        "has a row but is listed as unpublished"
                    } else {
                        "has no row (add one with its source to ROWS and docs/context-windows.md, or list it as unpublished)"
                    }
                ));
            }
        }
        assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    }

    #[test]
    fn claude_windows_follow_the_model_and_its_1m_option() {
        use AgentFamily::Claude;
        for id in [
            "claude:opus",
            "claude:sonnet",
            "claude:haiku",
            "claude:claude-fable-5-1[1m]",
        ] {
            assert_eq!(typical_window(Claude, id), Some(1_000_000), "{id}");
        }
        assert_eq!(typical_window(Claude, "claude-opus-4-6"), Some(200_000));
        assert_eq!(
            typical_window(Claude, "claude:claude-opus-4-6[1m]"),
            Some(1_000_000)
        );
        assert_eq!(
            typical_window(Claude, "claude-haiku-4-5-20251001"),
            Some(200_000)
        );
        assert_eq!(typical_window(Claude, "claude-haiku-5-5"), Some(1_000_000));
    }

    #[test]
    fn codex_reports_ninety_five_percent_of_the_listed_window() {
        for id in ["gpt-6-luna", "gpt-5.6-terra", "gpt-5.6-luna", "GPT-6.1-Sol"] {
            assert_eq!(
                typical_window(AgentFamily::Codex, id),
                Some(258_400),
                "{id}"
            );
        }
        assert_eq!(
            typical_window(AgentFamily::Codex, "gpt-5.3-codex-spark"),
            Some(121_600)
        );
    }

    #[test]
    fn cursor_ids_name_their_window_and_otherwise_fall_back_by_model() {
        use AgentFamily::Cursor;
        let id = |model: &str| format!("cursor:{model}");
        assert_eq!(
            typical_window(
                Cursor,
                &id("gpt-6-luna[context=272k,reasoning=low,fast=false]")
            ),
            Some(272_000)
        );
        assert_eq!(
            typical_window(
                Cursor,
                &id("claude-opus-5-5[context=300k,effort=medium,fast=false]")
            ),
            Some(300_000)
        );
        assert_eq!(
            typical_window(
                Cursor,
                &id("claude-opus-4-8[context=1m,effort=high,fast=false]")
            ),
            Some(1_000_000)
        );
        assert_eq!(
            typical_window(Cursor, &id("gpt-5.5-extra-high-fast")),
            Some(1_050_000)
        );
        assert_eq!(
            typical_window(Cursor, &id("grok-4.7-xhigh-fast")),
            Some(256_000)
        );
        assert_eq!(
            typical_window(Cursor, &id("cursor-grok-4.5-high")),
            Some(500_000)
        );
        assert_eq!(
            typical_window(Cursor, &id("claude-4.6-opus-max-thinking")),
            Some(200_000)
        );
        assert_eq!(typical_window(Cursor, &id("composer-2.5")), None);
    }

    #[test]
    fn opencode_models_read_past_their_provider() {
        use AgentFamily::OpenCode;
        assert_eq!(
            typical_window(OpenCode, "opencode:openai/gpt-5.6-sol-fast"),
            Some(1_050_000)
        );
        assert_eq!(
            typical_window(OpenCode, "opencode:google/gemini-3.7-flash"),
            Some(1_048_576)
        );
        assert_eq!(
            typical_window(OpenCode, "opencode:opencode/big-pickle"),
            Some(200_000)
        );
        assert_eq!(typical_window(OpenCode, "opencode:local/unheard-of"), None);
    }
}
