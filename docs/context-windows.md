# Context windows

The context popover shows `used / window`. The window is the provider's own
report when it gives one; otherwise the host fills in the model's **typical**
window from the table below and marks it `windowIsTypical` (the popover says
"Typical window for this model; the provider did not report one."). A model with
no published window stays unknown, and the popover says so in words.

Code: `crates/wonderd/src/context_windows.rs` (the table, as data),
`context_usage::typical_window` (when it applies), `history.rs` (where the
activity check carries it). Checked 2026-11-06.

Rules, in order:

1. A provider report (Codex rollout, Claude's SDK, ACP `usage_update`) always wins.
2. A model id that names its window wins next: Claude's `[1m]`, Cursor's `[context=272k,…]`.
3. Otherwise the table row for the model that ran (a Claude reply's model, else the chat's model).
   Effort, speed and thinking words (`-high`, `-fast`, `-thinking`, `-pro`, …) are dropped from the
   end of the name until a row matches. A trailing `*` in a pattern matches that model family.
4. A typical window smaller than the tokens already used is not shown (a larger variant ran).

## Sources

| Source | Where | Notes |
| --- | --- | --- |
| `claude-code-docs` | https://code.claude.com/docs/en/model-config ("Extended context"), https://platform.claude.com/docs/en/about-claude/models/overview | Fable 5.1, Sonnet 5 and later, Haiku 5.5 and Opus 4.7 and later run the 1M window by default, no `[1m]`. Opus 4.6 and Sonnet 4.6 reach 1M only through `[1m]`. |
| `codex-debug-models` | `codex debug models` (read-only): `context_window` 272000, `effective_context_window_percent` 95 | Codex reports 95% of the listed window: 258,400 (matches the rollout's `model_context_window`). `max_context_window` 872000 is not what a thread reports. |
| `models.dev` | https://models.dev/api.json `limit.context`; OpenCode's own copy `~/.cache/opencode/models.json` | API windows. The OpenAI/Google/xAI/first-party Kimi, GLM and Meta entries. |
| `cursor-docs` | https://cursor.com/docs/models; `cursor-agent --list-models` | Names the 1M models and Grok 4.7's 256k default; Cursor ids carry `[context=…]` when a variant is picked. |

Picker lists were read on 2026-11-06 with `scripts/context-window-pickers`
(`codex debug models`, `cursor-agent --list-models`, `opencode models`; the Claude list is the bridge's:
`opus`, `claude-fable-5-1[1m]`, `sonnet`, `haiku`). That output is the fixture
`crates/wonderd/src/context_windows_pickers.txt`; the test
`every_picker_model_has_a_window_or_is_listed_as_unpublished` fails for a listed model with no row.

To refresh: rerun the script into the fixture, run
`cargo test -p wonderd context_windows` (on a Boat box), add rows for what fails, then
`cargo test -p wonderd context_windows::tests::print_doc_rows -- --ignored --nocapture` prints the rows below.

## Table

| Pattern | Provider | Window (tokens) | Source | Checked |
| --- | --- | --- | --- | --- |
| `default` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `opus` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `sonnet` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `haiku` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `fable` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-fable-5*` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-opus-5*` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-opus-4-7` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-opus-4-8` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-sonnet-5*` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-haiku-5*` | any | 1000000 | claude-code-docs | 2026-11-06 |
| `claude-opus-4-6` | any | 200000 | claude-code-docs | 2026-11-06 |
| `claude-sonnet-4-6` | any | 200000 | claude-code-docs | 2026-11-06 |
| `claude-4.6-opus` | any | 200000 | claude-code-docs | 2026-11-06 |
| `claude-4.6-sonnet` | any | 200000 | claude-code-docs | 2026-11-06 |
| `claude-opus-4-5` | any | 200000 | models.dev | 2026-11-06 |
| `claude-sonnet-4-5` | any | 200000 | models.dev | 2026-11-06 |
| `claude-haiku-4-5` | any | 200000 | models.dev | 2026-11-06 |
| `claude-4.5-opus` | any | 200000 | models.dev | 2026-11-06 |
| `claude-4.5-sonnet` | any | 200000 | models.dev | 2026-11-06 |
| `claude-4-sonnet` | any | 200000 | models.dev | 2026-11-06 |
| `claude-sonnet-4` | any | 200000 | models.dev | 2026-11-06 |
| `claude-opus-4-1` | any | 200000 | models.dev | 2026-11-06 |
| `gpt-*` | Codex | 258400 | codex-debug-models | 2026-11-06 |
| `gpt-5.3-codex-spark` | Codex | 121600 | models.dev | 2026-11-06 |
| `gpt-6*` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-5.6*` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-5.5*` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-5.4` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-5.4-pro` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-5.4-mini` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5.4-nano` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5.3-codex` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5.3-codex-spark` | any | 128000 | models.dev | 2026-11-06 |
| `gpt-5.2*` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5.1*` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5-mini` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5-nano` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-5-pro` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-daybreak-blue-latest` | any | 1050000 | models.dev | 2026-11-06 |
| `gpt-daybreak-red-latest` | any | 400000 | models.dev | 2026-11-06 |
| `gpt-4.1*` | any | 1047576 | models.dev | 2026-11-06 |
| `gpt-4o*` | any | 128000 | models.dev | 2026-11-06 |
| `o3*` | any | 200000 | models.dev | 2026-11-06 |
| `gemini-3*` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-4*` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-2.5-flash` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-2.5-flash-lite` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-2.5-pro` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-2.5-computer-use-preview-10-2025` | any | 128000 | models.dev | 2026-11-06 |
| `gemini-flash-latest` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-flash-lite-latest` | any | 1048576 | models.dev | 2026-11-06 |
| `gemini-pro-latest` | any | 1048576 | models.dev | 2026-11-06 |
| `deep-research-preview-04-2026` | any | 131072 | models.dev | 2026-11-06 |
| `deep-research-max-preview-04-2026` | any | 131072 | models.dev | 2026-11-06 |
| `gemma-4-26b-a4b-it` | any | 262144 | models.dev | 2026-11-06 |
| `gemma-4-31b-it` | any | 262144 | models.dev | 2026-11-06 |
| `grok-4.7` | Cursor | 256000 | cursor-docs | 2026-11-06 |
| `grok-4.7` | any | 500000 | models.dev | 2026-11-06 |
| `grok-4.6` | any | 500000 | models.dev | 2026-11-06 |
| `grok-4.5` | any | 500000 | models.dev | 2026-11-06 |
| `kimi-k3` | any | 1048576 | models.dev | 2026-11-06 |
| `kimi-k2.7-code` | any | 262144 | models.dev | 2026-11-06 |
| `glm-5.2` | any | 1000000 | models.dev | 2026-11-06 |
| `deepseek-v41-flash-1017` | any | 1048576 | models.dev | 2026-11-06 |
| `muse-wonder-1.0` | any | 1048576 | models.dev | 2026-11-06 |
| `muse-spark-1.3` | any | 1048576 | models.dev | 2026-11-06 |
| `big-pickle` | OpenCode | 200000 | models.dev | 2026-11-06 |
| `beam-free` | OpenCode | 1048576 | models.dev | 2026-11-06 |
| `deepseek-v4.1-flash-free` | OpenCode | 200000 | models.dev | 2026-11-06 |
| `crouton-alpha-free` | OpenCode | 1048576 | models.dev | 2026-11-06 |
| `pulse-alpha-free` | OpenCode | 1048576 | models.dev | 2026-11-06 |
| `flint-40b-free` | OpenCode | 262144 | models.dev | 2026-11-06 |
| `glm-5.3-flash-free` | OpenCode | 200000 | models.dev | 2026-11-06 |
| `mimo-v2.6-flash-free` | OpenCode | 200000 | models.dev | 2026-11-06 |
| `nemotron-3.5-lightning-free` | OpenCode | 262144 | models.dev | 2026-11-06 |
| `nemotron-3.5-super-vl-free` | OpenCode | 1048576 | models.dev | 2026-11-06 |

## Picker coverage

Every picker model (variants of one model share a row) and the window the host returns for it.
`unknown` models have no published window: Cursor's Auto and Composer 2.5, and OpenCode's
`gemini-robotics-er-2-preview`, `chat-latest`, `gpt-5-search-api`, `gpt-audio-1.5`.

| Provider | Model | Picker entries | Window |
| --- | --- | --- | --- |
| claude | `opus` | 2 | 1,000,000 |
| claude | `claude-fable-5-1` | 1 | 1,000,000 |
| claude | `sonnet` | 2 | 1,000,000 |
| claude | `haiku` | 1 | 1,000,000 |
| claude | `claude-opus-5-5` | 1 | 1,000,000 |
| claude | `claude-sonnet-5-5` | 1 | 1,000,000 |
| claude | `claude-haiku-5-5` | 1 | 1,000,000 |
| claude | `claude-haiku-4-5-20251001` | 1 | 200,000 |
| codex | `gpt-6-luna` | 1 | 258,400 |
| codex | `gpt-5.6-terra` | 1 | 258,400 |
| codex | `gpt-5.6-luna` | 1 | 258,400 |
| cursor | `auto` | 1 | unknown |
| cursor | `gpt-5.3-codex` | 8 | 400,000 |
| cursor | `gpt-5.2` | 8 | 400,000 |
| cursor | `composer-2.5` | 2 | unknown |
| cursor | `claude-opus-5` | 16 | 1,000,000 |
| cursor | `gpt-5.6-sol` | 12 | 1,050,000 |
| cursor | `claude-fable-5` | 10 | 1,000,000 |
| cursor | `cursor-grok-4.5` | 6 | 500,000 |
| cursor | `gemini-3.7-flash` | 3 | 1,048,576 |
| cursor | `claude-sonnet-5` | 10 | 1,000,000 |
| cursor | `gpt-5.6-luna` | 12 | 1,050,000 |
| cursor | `grok-4.7` | 8 | 256,000 |
| cursor | `cursor-grok-4.6` | 8 | 500,000 |
| cursor | `claude-opus-5-5` | 10 | 1,000,000 |
| cursor | `claude-opus-4-8` | 20 | 1,000,000 |
| cursor | `gemini-4-argon` | 3 | 1,048,576 |
| cursor | `muse-wonder-1.0` | 6 | 1,048,576 |
| cursor | `gpt-5.5` | 10 | 1,050,000 |
| cursor | `claude-fable-5-1` | 10 | 1,000,000 |
| cursor | `gemini-3.8-flash` | 3 | 1,048,576 |
| cursor | `muse-spark-1.3` | 6 | 1,048,576 |
| cursor | `gpt-5.6-terra` | 12 | 1,050,000 |
| cursor | `claude-sonnet-5-5` | 5 | 1,000,000 |
| cursor | `claude-4.6-sonnet` | 2 | 200,000 |
| cursor | `claude-opus-4-7` | 20 | 1,000,000 |
| cursor | `gpt-5.4` | 7 | 1,050,000 |
| cursor | `claude-4.6-opus` | 4 | 200,000 |
| cursor | `claude-4.5-opus` | 2 | 200,000 |
| cursor | `gemini-3.6-flash` | 4 | 1,048,576 |
| cursor | `gemini-3.1` | 1 | 1,048,576 |
| cursor | `gpt-5.4-mini` | 5 | 400,000 |
| cursor | `gpt-5.4-nano` | 5 | 400,000 |
| cursor | `claude-haiku-5-5` | 10 | 1,000,000 |
| cursor | `claude-4.5-sonnet` | 2 | 200,000 |
| cursor | `gpt-5.1` | 3 | 400,000 |
| cursor | `gemini-3-flash` | 1 | 1,048,576 |
| cursor | `gemini-3.5-flash` | 1 | 1,048,576 |
| cursor | `claude-4-sonnet` | 2 | 200,000 |
| cursor | `gpt-5-mini` | 1 | 400,000 |
| cursor | `kimi-k3` | 3 | 1,048,576 |
| cursor | `kimi-k2.7-code` | 1 | 262,144 |
| cursor | `glm-5.2` | 2 | 1,000,000 |
| cursor | `deepseek-v41-flash-1017` | 5 | 1,048,576 |
| opencode | `google/deep-research-max-preview-04-2026` | 1 | 131,072 |
| opencode | `google/deep-research-preview-04-2026` | 1 | 131,072 |
| opencode | `google/gemini-2.5-computer-use-preview-10-2025` | 1 | 128,000 |
| opencode | `google/gemini-2.5-flash` | 1 | 1,048,576 |
| opencode | `google/gemini-2.5-flash-lite` | 1 | 1,048,576 |
| opencode | `google/gemini-2.5` | 1 | 1,048,576 |
| opencode | `google/gemini-3-flash-preview` | 1 | 1,048,576 |
| opencode | `google/gemini-3.1-flash-lite` | 1 | 1,048,576 |
| opencode | `google/gemini-3.1-pro-preview` | 1 | 1,048,576 |
| opencode | `google/gemini-3.1-pro-preview-customtools` | 1 | 1,048,576 |
| opencode | `google/gemini-3.5-flash` | 1 | 1,048,576 |
| opencode | `google/gemini-3.5-flash-lite` | 1 | 1,048,576 |
| opencode | `google/gemini-3.6-flash` | 1 | 1,048,576 |
| opencode | `google/gemini-3.7-flash` | 1 | 1,048,576 |
| opencode | `google/gemini-3.8-flash` | 1 | 1,048,576 |
| opencode | `google/gemini-4-argon` | 1 | 1,048,576 |
| opencode | `google/gemini-flash-latest` | 1 | 1,048,576 |
| opencode | `google/gemini-flash-lite-latest` | 1 | 1,048,576 |
| opencode | `google/gemini-pro-latest` | 1 | 1,048,576 |
| opencode | `google/gemini-robotics-er-2-preview` | 1 | unknown |
| opencode | `google/gemma-4-26b-a4b-it` | 1 | 262,144 |
| opencode | `google/gemma-4-31b-it` | 1 | 262,144 |
| opencode | `openai/chat-latest` | 1 | unknown |
| opencode | `openai/gpt-4.1` | 2 | 1,047,576 |
| opencode | `openai/gpt-4.1-mini` | 2 | 1,047,576 |
| opencode | `openai/gpt-4o` | 2 | 128,000 |
| opencode | `openai/gpt-4o-2024-08-06` | 1 | 128,000 |
| opencode | `openai/gpt-4o-2024-11-20` | 1 | 128,000 |
| opencode | `openai/gpt-4o-mini` | 2 | 128,000 |
| opencode | `openai/gpt-5` | 4 | 400,000 |
| opencode | `openai/gpt-5-mini` | 3 | 400,000 |
| opencode | `openai/gpt-5-nano` | 2 | 400,000 |
| opencode | `openai/gpt-5-search-api` | 1 | unknown |
| opencode | `openai/gpt-5.1` | 3 | 400,000 |
| opencode | `openai/gpt-5.2` | 4 | 400,000 |
| opencode | `openai/gpt-5.3-codex` | 2 | 400,000 |
| opencode | `openai/gpt-5.3-codex-spark` | 1 | 128,000 |
| opencode | `openai/gpt-5.4` | 3 | 1,050,000 |
| opencode | `openai/gpt-5.4-mini` | 3 | 400,000 |
| opencode | `openai/gpt-5.4-nano` | 2 | 400,000 |
| opencode | `openai/gpt-5.5` | 3 | 1,050,000 |
| opencode | `openai/gpt-5.6` | 3 | 1,050,000 |
| opencode | `openai/gpt-5.6-cyber` | 1 | 1,050,000 |
| opencode | `openai/gpt-5.6-luna` | 3 | 1,050,000 |
| opencode | `openai/gpt-5.6-sol` | 3 | 1,050,000 |
| opencode | `openai/gpt-5.6-terra` | 3 | 1,050,000 |
| opencode | `openai/gpt-6-astra` | 4 | 1,050,000 |
| opencode | `openai/gpt-6-luna` | 3 | 1,050,000 |
| opencode | `openai/gpt-6-sol` | 3 | 1,050,000 |
| opencode | `openai/gpt-6.1-sol` | 4 | 1,050,000 |
| opencode | `openai/gpt-6.1-terra` | 3 | 1,050,000 |
| opencode | `openai/gpt-audio-1.5` | 1 | unknown |
| opencode | `openai/gpt-daybreak-blue-latest` | 1 | 1,050,000 |
| opencode | `openai/gpt-daybreak-red-latest` | 1 | 400,000 |
| opencode | `openai/o3` | 4 | 200,000 |
| opencode | `opencode/beam-free` | 1 | 1,048,576 |
| opencode | `opencode/big-pickle` | 1 | 200,000 |
| opencode | `opencode/crouton-alpha-free` | 1 | 1,048,576 |
| opencode | `opencode/deepseek-v4.1-flash-free` | 1 | 200,000 |
| opencode | `opencode/flint-40b-free` | 1 | 262,144 |
| opencode | `opencode/glm-5.3-flash-free` | 1 | 200,000 |
| opencode | `opencode/mimo-v2.6-flash-free` | 1 | 200,000 |
| opencode | `opencode/nemotron-3.5-lightning-free` | 1 | 262,144 |
| opencode | `opencode/nemotron-3.5-super-vl-free` | 1 | 1,048,576 |
| opencode | `opencode/pulse-alpha-free` | 1 | 1,048,576 |
