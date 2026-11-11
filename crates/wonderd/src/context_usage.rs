//! Context usage reported by the harness; Wonder does not estimate it.
//!
//! Live token-usage notifications win over older persisted events. Codex has
//! no usage field on thread/read, so its native rollout remains a read-only
//! fallback. Ordinary Claude transcript replies omit the policy window, so a
//! newer reply reuses the window of the same model's earlier measured report;
//! with none, the older reading is kept and the phone shows it dimmed with its age.
//! Compaction and unknown reports invalidate older measurements across restarts.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const MAX_THREADS: usize = 256;
pub(crate) const UNREPORTED_WINDOW: u64 = 0;
#[cfg(test)]
const TAIL_BYTES: u64 = 512 * 1024;
const READ_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextUsage {
    pub used_tokens: u64,
    /// `UNREPORTED_WINDOW` when the provider gave the request's tokens but not its window.
    pub window_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Native event time for files; receipt time for notifications without a clock.
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Observation {
    at_ms: u64,
    usage: Option<ContextUsage>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    // The older schema did not retain invalidation and kept unscoped model
    // windows. Drop it instead of reviving an unverifiable old report.
    #[serde(default)]
    observations: VecDeque<(String, Observation)>,
}

static PATH: OnceLock<PathBuf> = OnceLock::new();
static LATEST: LazyLock<Mutex<Saved>> =
    LazyLock::new(|| Mutex::new(PATH.get().map(|path| load_saved(path)).unwrap_or_default()));

fn load_saved(path: &Path) -> Saved {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        .unwrap_or_default()
}

fn save_saved(path: &Path, saved: &Saved) {
    if let Ok(bytes) = serde_json::to_vec(saved) {
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(&temporary, path);
        }
    }
}

pub fn configure(data_dir: &Path) {
    let _ = PATH.set(data_dir.join("context-usage.json"));
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Usage-only events bypass the conversation inbox; compaction still reaches
/// history after invalidating the meter. An absent optional field is not a reset.
pub(crate) fn record_notification(notification: &serde_json::Value) -> bool {
    let method = notification
        .get("method")
        .and_then(serde_json::Value::as_str);
    let params = &notification["params"];
    let thread = params.get("threadId").and_then(serde_json::Value::as_str);
    let usage_event = method == Some("thread/tokenUsage/updated");
    let compacted = method == Some("thread/compacted")
        || (method == Some("item/completed")
            && params
                .pointer("/item/type")
                .and_then(serde_json::Value::as_str)
                == Some("contextCompaction"));
    if let Some(thread) = thread {
        if compacted || (usage_event && params.get("tokenUsage").is_some()) {
            let at_ms = now_ms();
            let usage = (!compacted)
                .then(|| parse(&params["tokenUsage"], at_ms))
                .flatten();
            record(thread, Observation { at_ms, usage });
        }
    }
    usage_event
}

/// An already-owned Claude Query supplied this snapshot. Failed/unknown
/// native reads stay unknown instead of falling back to an older file report.
pub(crate) fn native_read(thread: &str, snapshot: &serde_json::Value) -> Option<ContextUsage> {
    let at_ms = snapshot
        .get("observedAtMs")
        .and_then(serde_json::Value::as_u64);
    let usage = at_ms.and_then(|at| parse(&snapshot["tokenUsage"], at));
    // A snapshot without an event clock says nothing new: the saved reading
    // (or saved invalidation) stays instead of being replaced by an undated empty one.
    let Some(at_ms) = at_ms else {
        return reported(thread).and_then(|r| r.usage);
    };
    record(thread, Observation { at_ms, usage });
    reported(thread).and_then(|r| r.usage)
}

fn parse(usage: &serde_json::Value, at_ms: u64) -> Option<ContextUsage> {
    let window = usage
        .get("modelContextWindow")?
        .as_u64()
        .filter(|w| *w > 0)?;
    let used = usage.pointer("/last/totalTokens")?.as_u64()?;
    Some(ContextUsage {
        used_tokens: used,
        window_tokens: window,
        model: usage
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        at_ms,
    })
}

fn record(thread_id: &str, observation: Observation) {
    let mut saved = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    if saved.observations.iter().any(|(thread, previous)| {
        thread == thread_id && (previous.at_ms > observation.at_ms || previous == &observation)
    }) {
        return;
    }
    saved.observations.retain(|(thread, _)| thread != thread_id);
    saved
        .observations
        .push_back((thread_id.to_owned(), observation));
    while saved.observations.len() > MAX_THREADS {
        saved.observations.pop_front();
    }
    if let Some(path) = PATH.get() {
        save_saved(path, &saved);
    }
}

fn reported(thread_id: &str) -> Option<Observation> {
    LATEST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .observations
        .iter()
        .find(|(thread, _)| thread == thread_id)
        .map(|(_, observation)| observation.clone())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    CodexRollout(PathBuf),
    ClaudeTranscript(PathBuf),
}

impl Source {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::CodexRollout(path) | Self::ClaudeTranscript(path) => path,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FileReading {
    Unavailable,
    NoMeasurement,
    Incomplete,
    Sample(Observation),
    /// A Claude reply that reports request tokens but not the policy window.
    Reply {
        at_ms: u64,
        used_tokens: u64,
        model: String,
    },
}

static READERS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

pub(crate) async fn resolve(thread_id: &str, source: Option<Source>) -> Option<ContextUsage> {
    let reading = match source {
        None => FileReading::Unavailable,
        Some(source) => tokio::time::timeout(READ_DEADLINE, async {
            let permit = READERS.clone().acquire_owned().await.ok()?;
            tokio::task::spawn_blocking(move || {
                // Timed-out callers cannot leave unlimited blocking readers behind.
                let _permit = permit;
                read_source(source)
            })
            .await
            .ok()
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(FileReading::Incomplete),
    };
    // A live notification received during the read must still win. Persist
    // positive file invalidation too, so losing the source later cannot revive
    // a pre-compaction report after a restart.
    // A read that timed out or hit a partial file says nothing new: the saved
    // reading stays, and the phone shows it dimmed with its age.
    let Some(observation) = choose(reported(thread_id), reading) else {
        return reported(thread_id).and_then(|r| r.usage);
    };
    record(thread_id, observation);
    reported(thread_id).and_then(|r| r.usage)
}

fn choose(report: Option<Observation>, reading: FileReading) -> Option<Observation> {
    let sample = match reading {
        FileReading::Incomplete => return None,
        FileReading::Unavailable | FileReading::NoMeasurement => return report,
        FileReading::Sample(sample) => sample,
        FileReading::Reply {
            at_ms,
            used_tokens,
            model,
        } => {
            // Only the same model's earlier measured window is reused. Without
            // one the older reading stays (shown dimmed with its age).
            if report.as_ref().is_some_and(|r| r.at_ms >= at_ms) {
                return report;
            }
            let measured = report
                .as_ref()
                .and_then(|r| r.usage.as_ref())
                .filter(|u| u.window_tokens > 0);
            let window = measured
                .filter(|u| u.model.as_deref() == Some(model.as_str()))
                // A reply larger than that window means another variant (200K vs
                // 1M) of the same model; its window is unknown, not this one.
                .map(|u| u.window_tokens)
                .filter(|window| used_tokens <= *window);
            let reading = |window_tokens| {
                Some(Observation {
                    at_ms,
                    usage: Some(ContextUsage {
                        used_tokens,
                        window_tokens,
                        model: Some(model.clone()),
                        at_ms,
                    }),
                })
            };
            return match window {
                Some(window_tokens) => reading(window_tokens),
                // An older measured window stays (dimmed on the phone) rather
                // than being replaced by tokens alone.
                None if measured.is_some() => report,
                // No window was ever reported for this chat (a reply written by
                // an app Wonder does not run, T3 Code's Claude threads): the
                // request's own token count is still the provider's, so it
                // shows without a fill instead of nothing.
                None => reading(UNREPORTED_WINDOW),
            };
        }
    };
    if report.as_ref().is_some_and(|r| r.at_ms >= sample.at_ms) {
        return report;
    }
    Some(sample)
}

fn read_source(source: Source) -> FileReading {
    crate::desktop_activity::transcript::with(source.path(), |tail| tail.context(&source))
        .unwrap_or(FileReading::Unavailable)
}

/// Reduced at ingestion, alongside busy state and media offsets. Each provider
/// has its own projection; a path can be learned before its provider is known.
#[derive(Default)]
pub(crate) struct TranscriptUsage {
    codex: Option<FileReading>,
    claude: Option<FileReading>,
}

impl TranscriptUsage {
    pub(crate) fn accept(&mut self, entry: &serde_json::Value) {
        if let Some(sample) = codex_sample(entry) {
            self.codex = Some(sample);
        }
        if let Some(sample) = claude_sample(entry) {
            self.claude = Some(sample);
        }
    }

    pub(crate) fn incomplete(&mut self) {
        self.codex = Some(FileReading::Incomplete);
        self.claude = Some(FileReading::Incomplete);
    }

    pub(crate) fn reading(&self, source: &Source) -> FileReading {
        match source {
            Source::CodexRollout(_) => &self.codex,
            Source::ClaudeTranscript(_) => &self.claude,
        }
        .clone()
        .unwrap_or(FileReading::NoMeasurement)
    }
}

fn timestamp(value: &serde_json::Value) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()?
        .timestamp_millis()
        .try_into()
        .ok()
}

fn sample(value: &serde_json::Value, usage: Option<(u64, u64, Option<String>)>) -> FileReading {
    match timestamp(value) {
        Some(at_ms) => FileReading::Sample(Observation {
            at_ms,
            usage: usage.map(|(used_tokens, window_tokens, model)| ContextUsage {
                used_tokens,
                window_tokens,
                model,
                at_ms,
            }),
        }),
        None => FileReading::Incomplete,
    }
}

fn codex_sample(value: &serde_json::Value) -> Option<FileReading> {
    if value["type"] == "compacted" {
        return Some(sample(value, None));
    }
    if value["type"] != "event_msg" || value["payload"]["type"] != "token_count" {
        return None;
    }
    let info = &value["payload"]["info"];
    // Native token_count can carry only rate limits; absent info is no new
    // context measurement, whereas a present report with no window is unknown.
    if info.is_null() {
        return None;
    }
    let usage = || {
        let window = info
            .get("model_context_window")?
            .as_u64()
            .filter(|w| *w > 0)?;
        let used = info.pointer("/last_token_usage/total_tokens")?.as_u64()?;
        Some((used, window, None))
    };
    Some(sample(value, usage()))
}

fn claude_sample(value: &serde_json::Value) -> Option<FileReading> {
    if value["isSidechain"] == true {
        return None;
    }
    if value["type"] == "system" && value["subtype"] == "compact_boundary" {
        return Some(sample(value, None));
    }
    if value["type"] != "assistant" {
        return None;
    }
    if let Some(context) = value.get("context_usage") {
        let measured = || {
            Some((
                context.get("total_tokens")?.as_u64()?,
                context.get("raw_max_tokens")?.as_u64().filter(|w| *w > 0)?,
                Some(context.get("model")?.as_str()?.to_owned()),
            ))
        };
        return Some(sample(value, measured()));
    }
    let message = &value["message"];
    if message["model"]
        .as_str()
        .is_some_and(|m| m.starts_with('<'))
    {
        return None;
    }
    let usage = message.get("usage")?;
    // The window is borrowed in `choose` only from the same model's earlier
    // measured report; a reply with no usable model or tokens changes nothing.
    let tokens = |field: &str| usage.get(field).and_then(serde_json::Value::as_u64);
    let used_tokens = [
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ]
    .into_iter()
    .try_fold(tokens("input_tokens")?, |total, field| {
        total.checked_add(tokens(field).unwrap_or(0))
    })?;
    let (Some(at_ms), Some(model)) = (timestamp(value), message["model"].as_str()) else {
        return Some(FileReading::Incomplete);
    };
    Some(FileReading::Reply {
        at_ms,
        used_tokens,
        model: model.to_owned(),
    })
}

/// The window to show for a reading whose provider gave none: what the model
/// the chat runs typically has (`context_windows`), never smaller than the
/// reading itself. A model named with its own window ("[1m]") is believed over
/// the model id a reply carries, which does not say which variant ran.
pub(crate) fn typical_window(
    usage: &ContextUsage,
    family: wonder_store::AgentFamily,
    chat_model: Option<&str>,
) -> Option<u64> {
    if usage.window_tokens != UNREPORTED_WINDOW {
        return None;
    }
    let of =
        |model: Option<&str>| model.and_then(|m| crate::context_windows::typical_window(family, m));
    let named = chat_model.filter(|m| m.contains('['));
    of(named)
        .or_else(|| of(usage.model.as_deref()))
        .or_else(|| of(chat_model))
        .filter(|window| usage.used_tokens <= *window)
}

/// The activity check's `contextUsage`, or null while unknown. A reading whose
/// window nobody gave carries `windowTokens: 0`, never null: phones already
/// shipped decode the field as a non-optional integer (and show nothing for a
/// zero window), so a null would fail the whole activity decode. A window the
/// host filled in (`typical_window`) is marked `windowIsTypical`.
#[cfg(test)]
pub(crate) fn json(usage: Option<&ContextUsage>) -> serde_json::Value {
    json_with_typical(usage, None)
}

pub(crate) fn json_with_typical(
    usage: Option<&ContextUsage>,
    typical_window: Option<u64>,
) -> serde_json::Value {
    usage.map_or(serde_json::Value::Null, |usage| {
        let mut value = serde_json::json!({"usedTokens": usage.used_tokens, "windowTokens": usage.window_tokens, "atMs": usage.at_ms});
        if let Some(window) = typical_window {
            value["windowTokens"] = window.into();
            value["windowIsTypical"] = true.into();
        }
        value
    })
}

/// What the phone says when `contextUsage` is null: the provider has not
/// reported this chat's context window, in plain words, never an empty button.
pub(crate) fn note(family: wonder_store::AgentFamily) -> String {
    format!(
        "{} has not reported this chat's context yet. It shows once a reply reports it.",
        family.provider().display_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report(thread: &str, last: u64, window: Option<u64>) -> serde_json::Value {
        json!({"method": "thread/tokenUsage/updated", "params": {
            "threadId": thread, "turnId": "turn",
            "tokenUsage": {
                "last": {"totalTokens": last, "inputTokens": last, "cachedInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0},
                "total": {"totalTokens": last * 3, "inputTokens": last * 3, "cachedInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0},
                "modelContextWindow": window,
            }
        }})
    }

    #[tokio::test]
    async fn the_last_request_fills_the_window_and_the_newest_report_wins() {
        assert!(record_notification(&report("ctx-a", 40_000, Some(200_000))));
        assert!(record_notification(&report("ctx-a", 52_000, Some(200_000))));
        assert_eq!(
            json(resolve("ctx-a", None).await.as_ref()),
            json!({"usedTokens": 52_000, "windowTokens": 200_000, "atMs": reported("ctx-a").unwrap().at_ms}),
            "the cumulative total is not what the window holds"
        );
    }

    #[tokio::test]
    async fn unknown_windows_stay_unknown_and_other_notifications_pass_through() {
        assert!(record_notification(&report("ctx-b", 10_000, None)));
        assert_eq!(
            json(resolve("ctx-b", None).await.as_ref()),
            serde_json::Value::Null
        );
        assert_eq!(
            json(resolve("ctx-never", None).await.as_ref()),
            serde_json::Value::Null
        );
        assert!(!record_notification(
            &json!({"method": "turn/completed", "params": {"threadId": "ctx-c"}})
        ));
    }

    #[test]
    fn native_summary_preserves_zero_and_over_limit_counts() {
        for used in [0, 210_000] {
            let usage = native_read("ctx-native-summary", &json!({"observedAtMs": 10_000,
                "tokenUsage": {"last": {"totalTokens": used}, "modelContextWindow": 200_000, "model": "claude-native"}
            })).unwrap();
            assert_eq!(usage.used_tokens, used);
            assert_eq!(usage.at_ms, 10_000);
        }
        let saved = reported("ctx-native-summary").unwrap();
        assert_eq!(
            native_read("ctx-native-summary", &serde_json::Value::Null),
            saved.usage,
            "an undated failed poll keeps the saved reading"
        );
        assert_eq!(reported("ctx-native-summary"), Some(saved));
        record(
            "ctx-native-summary",
            Observation {
                at_ms: 20_000,
                usage: None,
            },
        );
        let invalidated = reported("ctx-native-summary").unwrap();
        assert_eq!(
            native_read(
                "ctx-native-summary",
                &json!({"observedAtMs": 10_000,
                    "tokenUsage": {"last": {"totalTokens": 1234}, "modelContextWindow": 200_000}
                })
            ),
            None,
            "an older native snapshot cannot revive a newer invalidation"
        );
        assert_eq!(
            native_read("ctx-native-summary", &serde_json::Value::Null),
            None
        );
        assert_eq!(
            reported("ctx-native-summary"),
            Some(invalidated),
            "repeated unknown reads are not new measurements or persistence writes"
        );
        record(
            "ctx-native-unavailable",
            Observation {
                at_ms: 1,
                usage: None,
            },
        );
        assert_eq!(
            native_read("ctx-native-unavailable", &serde_json::Value::Null),
            None
        );
        assert_eq!(
            reported("ctx-native-unavailable").unwrap().at_ms,
            1,
            "a later failed poll must not redate an existing invalidation"
        );
    }

    #[tokio::test]
    async fn explicit_unknown_usage_replaces_a_previous_report() {
        record_notification(&report("ctx-invalidated", 52_000, Some(200_000)));
        record_notification(&report("ctx-invalidated", 52_000, None));
        assert_eq!(resolve("ctx-invalidated", None).await, None);
    }

    #[tokio::test]
    async fn native_compaction_clears_usage_without_consuming_the_history_event() {
        record_notification(&report("ctx-compacted", 52_000, Some(200_000)));
        assert!(!record_notification(
            &json!({"method": "item/completed", "params": {
                "threadId": "ctx-compacted", "item": {"type": "contextCompaction", "id": "compact"}
            }})
        ));
        assert_eq!(resolve("ctx-compacted", None).await, None);
    }

    /// A reply written in the Codex app (or before a restart) still shows:
    /// the rollout's newest `token_count` is read.
    #[tokio::test]
    async fn a_codex_rollout_reports_its_last_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let event = |last: u64| {
            json!({"timestamp": "1970-01-01T00:00:02Z", "type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": {"total_tokens": 900_000},
            "last_token_usage": {"total_tokens": last}, "model_context_window": 258_400}}})
            .to_string()
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                event(40_000),
                json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}),
                event(84_000)
            ),
        )
        .unwrap();
        assert_eq!(
            json(
                resolve("ctx-codex-file", Some(Source::CodexRollout(path.clone())))
                    .await
                    .as_ref()
            ),
            json!({"usedTokens": 84_000, "windowTokens": 258_400, "atMs": 2_000})
        );
        let mut zero: serde_json::Value = serde_json::from_str(&event(0)).unwrap();
        zero["timestamp"] = json!("1970-01-01T00:00:03Z");
        std::fs::write(&path, format!("{zero}\n")).unwrap();
        assert_eq!(
            json(
                resolve("ctx-codex-file", Some(Source::CodexRollout(path)))
                    .await
                    .as_ref()
            ),
            json!({"usedTokens": 0, "windowTokens": 258_400, "atMs": 3_000})
        );
    }

    /// A model name does not prove its next request's compaction policy window.
    #[tokio::test]
    async fn a_claude_transcript_needs_its_own_native_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let source = Source::ClaudeTranscript(path.clone());
        let reply = json!({"timestamp": "1970-01-01T00:00:02Z", "type": "assistant", "message": {
            "model": "claude-test-model", "usage": {"input_tokens": 10, "cache_read_input_tokens": 70_000,
            "cache_creation_input_tokens": 990, "output_tokens": 500}}});
        std::fs::write(&path, format!("{reply}\n")).unwrap();
        let usage = ContextUsage {
            used_tokens: 1_000,
            window_tokens: 1_000_000,
            model: Some("claude-test-model".into()),
            at_ms: 1_000,
        };
        record(
            "ctx-other-thread",
            Observation {
                at_ms: 1_000,
                usage: Some(usage.clone()),
            },
        );
        let unreported = resolve("ctx-claude-file", Some(source.clone()))
            .await
            .unwrap();
        assert_eq!(
            (unreported.used_tokens, unreported.window_tokens),
            (71_500, UNREPORTED_WINDOW),
            "a chat with no measured window still shows the reply's own tokens"
        );
        for (thread, model, borrowed) in [
            ("ctx-model-less", None, false),
            ("ctx-model-other", Some("another-model".into()), false),
            ("ctx-model-exact", Some("claude-test-model".into()), true),
        ] {
            record(
                thread,
                Observation {
                    at_ms: 1_000,
                    usage: Some(ContextUsage {
                        model,
                        ..usage.clone()
                    }),
                },
            );
            let shown = resolve(thread, Some(source.clone())).await.unwrap();
            if borrowed {
                assert_eq!(
                    (shown.used_tokens, shown.window_tokens, shown.at_ms),
                    (71_500, 1_000_000, 2_000)
                );
            } else {
                assert_eq!(
                    shown,
                    usage_for(thread),
                    "no same-model window: the older reading stays, with its age"
                );
            }
        }
        let native = json!({"timestamp": "1970-01-01T00:00:03Z", "type": "assistant", "context_usage": {
            "total_tokens": 400, "raw_max_tokens": 180_000, "model": "claude-new-model"
        }, "message": {"model": "<synthetic>"}});
        std::fs::write(&path, format!("{native}\n")).unwrap();
        let measured = resolve("ctx-claude-exact", Some(source.clone()))
            .await
            .unwrap();
        assert_eq!(
            json(Some(&measured)),
            json!({"usedTokens": 400, "windowTokens": 180_000, "atMs": 3_000})
        );
        assert_eq!(measured.at_ms, 3_000);
        // Mentioning compaction inside user text is not a compaction event.
        let mention = json!({"type": "user", "message": {"content": "\"compact_boundary\""}});
        std::fs::write(&path, format!("{native}\n{mention}\n")).unwrap();
        assert_eq!(
            resolve("ctx-claude-exact", Some(source.clone())).await,
            Some(measured)
        );
        let compact = json!({"timestamp": "1970-01-01T00:00:04Z", "type": "system", "subtype": "compact_boundary"});
        std::fs::write(&path, format!("{native}\n{compact}\n")).unwrap();
        assert_eq!(
            resolve("ctx-claude-exact", Some(source)).await,
            None,
            "compaction supersedes even a positive native context report"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            resolve("ctx-claude-exact", None).await,
            None,
            "a persisted file invalidation survives losing the source"
        );
    }

    fn usage_for(thread: &str) -> ContextUsage {
        reported(thread).unwrap().usage.unwrap()
    }

    fn codex_event(used: u64) -> serde_json::Value {
        json!({"timestamp": "1970-01-01T00:00:02Z", "type": "event_msg", "payload": {"type": "token_count",
            "info": {"last_token_usage": {"total_tokens": used}, "model_context_window": 200_000}}})
    }

    #[tokio::test]
    async fn unrelated_appends_do_not_redate_usage_and_invalid_newer_events_do_not_resurrect_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let source = Source::CodexRollout(path.clone());
        let initial = codex_event(10_000);
        std::fs::write(&path, format!("{initial}\n")).unwrap();
        let before = resolve("ctx-append", Some(source.clone())).await.unwrap();
        assert_eq!(before.at_ms, 2_000);
        let unrelated = json!({"timestamp": "1970-01-01T00:00:09Z", "type": "event_msg", "payload": {"type": "agent_message", "message": "Done"}});
        std::fs::write(&path, format!("{initial}\n{unrelated}\n")).unwrap();
        assert_eq!(
            resolve("ctx-append", Some(source.clone())).await,
            Some(before.clone())
        );
        record(
            "ctx-append",
            Observation {
                at_ms: 3_000,
                usage: Some(ContextUsage {
                    used_tokens: 20_000,
                    at_ms: 3_000,
                    ..before
                }),
            },
        );
        assert_eq!(
            resolve("ctx-append", Some(source.clone()))
                .await
                .unwrap()
                .used_tokens,
            20_000,
            "an unrelated file write cannot overrule a newer native notification"
        );
        for invalid in [
            json!({"timestamp": "1970-01-01T00:00:04Z", "type": "compacted", "payload": {}}),
            json!({"timestamp": "1970-01-01T00:00:04Z", "type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"total_tokens": 100}, "model_context_window": null}}}),
        ] {
            std::fs::write(&path, format!("{initial}\n{invalid}\n")).unwrap();
            assert_eq!(resolve("ctx-append", Some(source.clone())).await, None);
        }
        let mut next = codex_event(9_000);
        next["timestamp"] = json!("1970-01-01T00:00:05Z");
        std::fs::write(&path, format!("{next}\n")).unwrap();
        assert_eq!(
            resolve("ctx-append", Some(source))
                .await
                .unwrap()
                .used_tokens,
            9_000
        );
    }

    #[tokio::test]
    async fn missing_optional_fields_preserve_usage_but_partial_files_stay_unknown() {
        let thread = "ctx-partial";
        record_notification(&report(thread, 20_000, Some(200_000)));
        record_notification(
            &json!({"method": "thread/tokenUsage/updated", "params": {"threadId": thread}}),
        );
        assert_eq!(resolve(thread, None).await.unwrap().used_tokens, 20_000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let source = Source::CodexRollout(path.clone());
        let good = codex_event(10_000);
        let mut missing_clock = good.clone();
        missing_clock.as_object_mut().unwrap().remove("timestamp");
        for contents in [
            format!("{good}\n{{\"type\": "),
            format!("{good}\nnot-json\n"),
            format!("{missing_clock}\n"),
            format!("{good}\n{}\n", " ".repeat(TAIL_BYTES as usize)),
        ] {
            std::fs::write(&path, contents).unwrap();
            // A partial file says nothing new: any saved reading stays.
            assert_eq!(
                resolve(thread, Some(source.clone())).await,
                reported(thread).and_then(|r| r.usage)
            );
        }
        record_notification(
            &json!({"method": "thread/tokenUsage/updated", "params": {"threadId": thread, "tokenUsage": null}}),
        );
        assert_eq!(resolve(thread, None).await, None);
        // Invalidation remains explicit when saved and loaded; old schema windows
        // are discarded because they have no thread/profile association.
        let saved = Saved {
            observations: VecDeque::from([(thread.into(), reported(thread).unwrap())]),
        };
        let restored: Saved = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(restored.observations[0].1.usage.is_none());
        let legacy: Saved = serde_json::from_value(
            json!({"models": {"claude-test-model": 1_000_000}, "threads": []}),
        )
        .unwrap();
        assert!(legacy.observations.is_empty());
    }
    fn claude_reply(
        at: &str,
        model: &str,
        input: u64,
        cached: u64,
        output: u64,
    ) -> serde_json::Value {
        json!({"timestamp": at, "type": "assistant", "message": {"model": model, "usage": {
            "input_tokens": input, "cache_read_input_tokens": cached,
            "cache_creation_input_tokens": 0, "output_tokens": output}}})
    }

    /// A T3 Code Claude child: its transcript has replies and no `context_usage`
    /// (measured on a real T3 transcript: 573 usage lines, 0 context_usage), and
    /// Wonder never owned its Query, so no window was ever reported.
    #[tokio::test]
    async fn a_t3_code_claude_child_shows_request_tokens_until_a_window_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t3-claude.jsonl");
        let source = Source::ClaudeTranscript(path.clone());
        let first = claude_reply("1970-01-01T00:00:02Z", "claude-t3", 5, 40_000, 100);
        std::fs::write(&path, format!("{first}\n")).unwrap();
        let shown = resolve("ctx-t3-claude", Some(source.clone()))
            .await
            .unwrap();
        assert_eq!(
            json(Some(&shown)),
            json!({"usedTokens": 40_105, "windowTokens": 0, "atMs": 2_000}),
            "the cumulative output of the whole session is not summed in; only the last request"
        );
        // A later reply replaces the tokens-only reading instead of keeping the older one.
        let second = claude_reply("1970-01-01T00:00:05Z", "claude-t3", 5, 52_000, 200);
        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let newer = resolve("ctx-t3-claude", Some(source.clone()))
            .await
            .unwrap();
        assert_eq!((newer.used_tokens, newer.at_ms), (52_205, 5_000));
        // Compaction clears it, and the next reply starts a new reading.
        let compact = json!({"timestamp": "1970-01-01T00:00:06Z", "type": "system", "subtype": "compact_boundary"});
        std::fs::write(&path, format!("{first}\n{second}\n{compact}\n")).unwrap();
        assert_eq!(resolve("ctx-t3-claude", Some(source.clone())).await, None);
        let third = claude_reply("1970-01-01T00:00:08Z", "claude-t3", 5, 9_000, 50);
        std::fs::write(&path, format!("{first}\n{second}\n{compact}\n{third}\n")).unwrap();
        assert_eq!(
            resolve("ctx-t3-claude", Some(source.clone()))
                .await
                .unwrap()
                .used_tokens,
            9_055
        );
        // A measured report for the same model (Wonder ran a turn itself) gives it a window.
        assert!(record_notification(
            &json!({"method": "thread/tokenUsage/updated", "params": {
            "threadId": "ctx-t3-claude", "tokenUsage": {"last": {"totalTokens": 9_100}, "total": {"totalTokens": 90_000},
            "modelContextWindow": 200_000, "model": "claude-t3"}}})
        ));
        let fourth = claude_reply("1970-01-01T00:00:00Z", "claude-t3", 1, 1, 1);
        std::fs::write(&path, format!("{fourth}\n")).unwrap();
        assert_eq!(usage_for("ctx-t3-claude").window_tokens, 200_000);
    }

    /// A T3 Code Codex child's rollout carries the window, so it reports in full.
    #[tokio::test]
    async fn a_t3_code_codex_child_reports_from_its_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t3-codex.jsonl");
        std::fs::write(&path, format!("{}\n", codex_event(31_000))).unwrap();
        let shown = resolve("ctx-t3-codex", Some(Source::CodexRollout(path)))
            .await
            .unwrap();
        assert_eq!(
            json(Some(&shown)),
            json!({"usedTokens": 31_000, "windowTokens": 200_000, "atMs": 2_000}),
            "last_token_usage, not the thread's running total"
        );
    }

    /// OpenCode and Cursor report through ACP `usage_update`: `used` is the
    /// context size and `size` its window, both live notifications.
    #[tokio::test]
    async fn acp_providers_report_their_context_from_usage_updates() {
        for thread in ["ctx-opencode-fixture", "ctx-cursor-fixture"] {
            assert!(record_notification(
                &json!({"method": "thread/tokenUsage/updated", "params": {
                "threadId": thread, "tokenUsage": {"last": {"totalTokens": 12_000}, "total": {"totalTokens": 12_000},
                "modelContextWindow": 128_000}}})
            ));
            assert_eq!(
                json(resolve(thread, None).await.as_ref()),
                json!({"usedTokens": 12_000, "windowTokens": 128_000, "atMs": reported(thread).unwrap().at_ms})
            );
        }
    }

    /// The stored reading is what a restarted host returns on open.
    #[test]
    fn saved_readings_survive_a_host_restart_including_tokens_only_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context-usage.json");
        let mut saved = Saved::default();
        for (thread, window) in [("full", 200_000), ("tokens-only", UNREPORTED_WINDOW)] {
            saved.observations.push_back((
                thread.to_owned(),
                Observation {
                    at_ms: 7_000,
                    usage: Some(ContextUsage {
                        used_tokens: 1_234,
                        window_tokens: window,
                        model: None,
                        at_ms: 7_000,
                    }),
                },
            ));
        }
        save_saved(&path, &saved);
        let restored = load_saved(&path);
        assert_eq!(restored.observations, saved.observations);
        assert_eq!(
            json(restored.observations[1].1.usage.as_ref()),
            json!({"usedTokens": 1_234, "windowTokens": 0, "atMs": 7_000})
        );
        assert!(load_saved(&dir.path().join("missing.json"))
            .observations
            .is_empty());
    }

    #[test]
    fn a_provider_without_a_reading_says_so_in_its_own_words() {
        use wonder_store::AgentFamily;
        for family in AgentFamily::ALL {
            let note = note(family);
            assert!(note.starts_with(family.provider().display_name), "{note}");
            assert!(note.contains("context"), "{note}");
        }
    }

    #[test]
    fn a_missing_window_is_filled_from_the_model_and_labeled_typical() {
        use wonder_store::AgentFamily::{Claude, Codex, Cursor};
        let reading = |used_tokens, window_tokens, model: Option<&str>| ContextUsage {
            used_tokens,
            window_tokens,
            model: model.map(str::to_owned),
            at_ms: 1,
        };
        // A reply names the model that ran; the chat's own model backs it up.
        let tokens_only = reading(40_105, UNREPORTED_WINDOW, Some("claude-opus-5-5"));
        assert_eq!(
            typical_window(&tokens_only, Claude, Some("claude:opus")),
            Some(1_000_000)
        );
        assert_eq!(
            json_with_typical(Some(&tokens_only), Some(1_000_000)),
            json!({"usedTokens": 40_105, "windowTokens": 1_000_000, "windowIsTypical": true, "atMs": 1})
        );
        let acp = reading(12_000, UNREPORTED_WINDOW, None);
        assert_eq!(
            typical_window(
                &acp,
                Cursor,
                Some("cursor:gpt-6-luna[context=272k,reasoning=low]")
            ),
            Some(272_000)
        );
        assert_eq!(
            typical_window(&acp, Codex, Some("gpt-6-luna")),
            Some(258_400)
        );
        // The provider's own window always wins, and a model nobody publishes stays unknown.
        let reported = reading(12_000, 200_000, None);
        assert_eq!(typical_window(&reported, Codex, Some("gpt-6-luna")), None);
        assert_eq!(
            json_with_typical(Some(&reported), None)["windowTokens"],
            200_000
        );
        assert_eq!(
            typical_window(&acp, Cursor, Some("cursor:composer-2.5")),
            None
        );
        // A reply bigger than the typical window ran the larger variant, so no label claims it.
        let large = reading(300_000, UNREPORTED_WINDOW, Some("claude-opus-4-6"));
        assert_eq!(typical_window(&large, Claude, Some("claude:opus")), None);
        assert_eq!(
            typical_window(&large, Claude, Some("claude:claude-opus-4-6")),
            None
        );
        assert_eq!(
            typical_window(&large, Claude, Some("claude:claude-opus-4-6[1m]")),
            Some(1_000_000)
        );
    }
}
