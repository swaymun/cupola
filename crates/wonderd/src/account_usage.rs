//! Sanitized account usage for the owner-facing settings surface.
use super::*;
use serde_json::{json, Value};

const MAX_WINDOWS: usize = 8;
const MAX_INSPECTED_LEGACY_WINDOWS: usize = 64;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageWindow {
    id: String,
    label: String,
    used_percent: f64,
    remaining_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_duration_mins: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct UsageQuery {
    #[serde(default)]
    agent_family: AgentFamily,
}

pub(super) async fn read(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<UsageQuery>,
) -> Response {
    // Account usage belongs to the owner's provider home used by Project threads.
    respond(
        projects::rpc_for(&state, query.agent_family).await,
        query.agent_family,
    )
    .await
}

/// Sanitized usage for the computer's own sign-in, as the phone reads it.
pub(crate) async fn respond(
    rpc: Result<wonder_app_server::RpcClient, String>,
    family: AgentFamily,
) -> Response {
    let Ok(rpc) = rpc else {
        return unavailable(family);
    };
    // Codex names its account in a second call; ask both at once, and give up on
    // the name quickly so a slow account/read never delays the limits.
    let (result, account) =
        tokio::join!(rpc.request("account/rateLimits/read", json!({})), async {
            if family == AgentFamily::Claude {
                return None;
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                rpc.request("account/read", json!({})),
            )
            .await
            .ok()
        });
    if result
        .as_ref()
        .ok()
        .and_then(|response| response.error.as_ref())
        .is_some_and(|error| error.code == -32601)
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            [(header::CACHE_CONTROL, "no-store")],
            "This provider does not offer usage details in Cupola yet.",
        )
            .into_response();
    }
    let Some(runtime) = result
        .ok()
        .filter(|response| response.error.is_none())
        .and_then(|response| response.result)
    else {
        return unavailable(family);
    };
    let Some(windows) = (if family == AgentFamily::Claude {
        project_claude_windows(&runtime)
    } else {
        project_windows(&runtime)
    }) else {
        return unavailable(family);
    };
    let email = account_email(&runtime, account, family);
    let (account_key, account_label) = account_identity(family, email.as_deref());
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "agentFamily": family,
            "accountKey": account_key,
            "accountLabel": account_label,
            "checkedAtMs": now_ms(),
            "additionalUsageAvailable": additional_usage_available(&runtime, family),
            "planType": plan_type(&runtime, family),
            "windows": windows,
        })),
    )
        .into_response()
}

/// The email of the provider sign-in this computer uses, when the provider names it.
fn account_email(
    runtime: &Value,
    account: Option<Result<wonder_app_server::RpcResponse, wonder_app_server::RuntimeError>>,
    family: AgentFamily,
) -> Option<String> {
    let value = if family == AgentFamily::Claude {
        runtime.get("accountEmail").cloned()
    } else {
        let account = account?.ok()?.result?;
        account.pointer("/account/email").cloned()
    }?;
    value
        .as_str()
        .map(str::trim)
        .filter(|email| !email.is_empty() && email.len() <= 200)
        .map(str::to_owned)
}

/// A stable key that is the same on every computer signed in to one provider
/// account, plus the email to show. Both are absent when the provider names no
/// account, so the phone keeps that computer's usage separate.
fn account_identity(family: AgentFamily, email: Option<&str>) -> (Option<String>, Option<String>) {
    use sha2::{Digest, Sha256};
    let Some(email) = email else {
        return (None, None);
    };
    let digest = Sha256::digest(format!("{family:?}:{}", email.to_lowercase()));
    (Some(hex::encode(&digest[..8])), Some(email.to_owned()))
}

/// When every exhausted usage window `rpc`'s account reports has reset (Unix
/// seconds): the latest reset among windows at 100%. `None` when no window is
/// exhausted or none says when it resets.
pub(crate) async fn exhausted_until(
    rpc: &wonder_app_server::RpcClient,
    family: AgentFamily,
) -> Option<i64> {
    let response = rpc
        .request("account/rateLimits/read", json!({}))
        .await
        .ok()?;
    let runtime = response.result.filter(|_| response.error.is_none())?;
    let windows = if family == AgentFamily::Claude {
        project_claude_windows(&runtime)
    } else {
        project_windows(&runtime)
    }?;
    latest_exhausted_reset(&windows)
}

fn latest_exhausted_reset(windows: &[UsageWindow]) -> Option<i64> {
    windows
        .iter()
        .filter(|window| window.used_percent >= 100.0)
        .filter_map(|window| window.resets_at)
        // Some providers report milliseconds.
        .map(|at| if at > 100_000_000_000 { at / 1000 } else { at } as i64)
        .max()
}

fn additional_usage_available(runtime: &Value, family: AgentFamily) -> Option<bool> {
    if family == AgentFamily::Claude {
        return runtime
            .get("additionalUsageAvailable")
            .and_then(Value::as_bool);
    }
    let preferred = runtime
        .pointer("/rateLimitsByLimitId/codex")
        .filter(|source| !project_source(source).is_empty());
    let source = preferred.or_else(|| runtime.get("rateLimits"))?;
    let credits = source.get("credits")?;
    match (
        credits["hasCredits"].as_bool(),
        credits["unlimited"].as_bool(),
    ) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// The plan the provider names for the account ("pro", "max"), only when reported.
fn plan_type(runtime: &Value, family: AgentFamily) -> Option<String> {
    let value = if family == AgentFamily::Claude {
        runtime.get("planType")
    } else {
        runtime
            .pointer("/rateLimitsByLimitId/codex/planType")
            .filter(|plan| plan.is_string())
            .or_else(|| runtime.pointer("/rateLimits/planType"))
    }?;
    value
        .as_str()
        .map(str::trim)
        .filter(|plan| !plan.is_empty() && plan.len() <= 40)
        .map(str::to_owned)
}

fn unavailable(family: AgentFamily) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        match family {
            AgentFamily::Codex => "Codex usage is temporarily unavailable. Check Codex on your computer, then refresh.",
            AgentFamily::Claude => "Claude usage is temporarily unavailable. Check your Claude subscription on your computer, then refresh.",
            AgentFamily::OpenCode => "OpenCode does not report usage limits.",
            AgentFamily::Cursor => "Cursor does not report usage limits.",
        },
    )
        .into_response()
}

fn project_claude_windows(runtime: &Value) -> Option<Vec<UsageWindow>> {
    let mut windows = Vec::new();
    let values = runtime.get("windows")?.as_array()?;
    for id in [
        "five_hour",
        "seven_day",
        "seven_day_sonnet",
        "seven_day_opus",
    ] {
        let Some(value) = values
            .iter()
            .take(MAX_INSPECTED_LEGACY_WINDOWS)
            .find(|v| v["id"] == id)
        else {
            continue;
        };
        if let Some(mut window) = project_window(value, id.into()) {
            window.window_duration_mins = Some(if id == "five_hour" { 300 } else { 10_080 });
            window.label = match id {
                "five_hour" => "5 hours",
                "seven_day_sonnet" => "Weekly · Sonnet",
                "seven_day_opus" => "Weekly · Opus",
                _ => "Weekly",
            }
            .into();
            windows.push(window);
        }
    }
    // Per-model weekly windows Claude reports by name ("Weekly · Fable").
    for value in values.iter().take(MAX_INSPECTED_LEGACY_WINDOWS) {
        let Some(id) = value["id"].as_str().filter(|id| {
            id.strip_prefix("seven_day_model_").is_some_and(|slug| {
                !slug.is_empty()
                    && slug.len() <= 23
                    && slug
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
        }) else {
            continue;
        };
        let Some(label) = value["label"]
            .as_str()
            .filter(|label| label.starts_with("Weekly · ") && label.chars().count() <= 32)
        else {
            continue;
        };
        if windows.len() >= MAX_WINDOWS || windows.iter().any(|w| w.id == id) {
            continue;
        }
        if let Some(mut window) = project_window(value, id.into()) {
            window.window_duration_mins = Some(10_080);
            window.label = label.into();
            windows.push(window);
        }
    }
    (!windows.is_empty()).then_some(windows)
}

fn project_windows(runtime: &Value) -> Option<Vec<UsageWindow>> {
    let preferred = runtime
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .and_then(|limits| limits.get("codex"));
    if let Some(source) = preferred {
        let windows = project_source(source);
        if !windows.is_empty() {
            return Some(windows);
        }
    }
    if let Some(source) = runtime
        .get("rateLimits")
        .filter(|source| source.is_object() || source.is_array())
    {
        let windows = project_source(source);
        if !windows.is_empty()
            || source.as_array().is_some_and(Vec::is_empty)
            || source.as_object().is_some_and(serde_json::Map::is_empty)
        {
            return Some(windows);
        }
    }
    preferred
        .filter(|source| {
            source.as_array().is_some_and(Vec::is_empty)
                || source.as_object().is_some_and(serde_json::Map::is_empty)
        })
        .map(project_source)
}

fn project_source(source: &Value) -> Vec<UsageWindow> {
    if let Some(legacy) = source.as_array() {
        let mut windows = Vec::new();
        for value in legacy.iter().take(MAX_INSPECTED_LEGACY_WINDOWS) {
            let Some(mut window) = project_window(value, String::new()) else {
                continue;
            };
            window.id = format!("window-{}", windows.len() + 1);
            windows.push(window);
            if windows.len() == MAX_WINDOWS {
                break;
            }
        }
        return windows;
    }
    ["primary", "secondary"]
        .iter()
        .filter_map(|id| {
            source
                .get(id)
                .and_then(|value| project_window(value, (*id).to_owned()))
        })
        .take(MAX_WINDOWS)
        .collect()
}

fn project_window(value: &Value, id: String) -> Option<UsageWindow> {
    let used_percent = value.get("usedPercent")?.as_f64()?;
    if !used_percent.is_finite() {
        return None;
    }
    let used_percent = used_percent.clamp(0.0, 100.0);
    let window_duration_mins = value
        .get("windowDurationMins")
        .and_then(Value::as_u64)
        .filter(|minutes| *minutes > 0);
    let resets_at = value
        .get("resetsAt")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0);
    Some(UsageWindow {
        id,
        label: label_for(window_duration_mins),
        used_percent,
        remaining_percent: 100.0 - used_percent,
        window_duration_mins,
        resets_at,
    })
}

fn label_for(duration_mins: Option<u64>) -> String {
    match duration_mins {
        Some(60) => "Hourly".to_owned(),
        Some(1_440) => "Daily".to_owned(),
        Some(300) => "5 hours".to_owned(),
        Some(10_080) => "Weekly".to_owned(),
        Some(minutes) if minutes % 60 == 0 && minutes / 60 <= 24 => {
            format!("{} hours", minutes / 60)
        }
        _ => "Usage".to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn exhausted_reset_is_the_latest_reset_among_full_windows() {
        let windows = project(json!({"rateLimits": {
            "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1_700_000_000},
            "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1_700_100_000_000_u64}
        }}));
        assert_eq!(latest_exhausted_reset(&windows), Some(1_700_100_000));
        let windows = project(json!({"rateLimits": {
            "primary": {"usedPercent": 40, "windowDurationMins": 300, "resetsAt": 1_700_000_000}
        }}));
        assert_eq!(latest_exhausted_reset(&windows), None);
    }

    fn project(value: Value) -> Vec<UsageWindow> {
        project_windows(&value).unwrap_or_default()
    }

    #[test]
    fn claude_windows_are_provider_scoped_and_missing_usage_is_not_zero() {
        assert!(project_claude_windows(&json!({"windows":[]})).is_none());
        let windows = project_claude_windows(&json!({"windows":[{"id":"five_hour","usedPercent":20,"remainingPercent":999,"resetsAt":1700000000},{"id":"seven_day","usedPercent":3},{"id":"future_secret","usedPercent":12}]})).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].remaining_percent, 80.0);
        assert_eq!(windows[0].resets_at, Some(1700000000));
        assert_eq!(windows[1].label, "Weekly");
        crate::tests::validate_http_contract(
            "accountUsage",
            &json!({"agentFamily":"claude", "checkedAtMs":1, "windows": windows}),
        );
        // A per-model weekly window keeps Claude's name; a malformed one is dropped.
        let windows = project_claude_windows(&json!({"windows":[
            {"id":"seven_day","usedPercent":2},
            {"id":"seven_day_model_fable","label":"Weekly · Fable","usedPercent":0,"resetsAt":1700000000},
            {"id":"seven_day_model_Bad!","label":"Weekly · Bad","usedPercent":1},
            {"id":"seven_day_model_x","label":"Secret","usedPercent":1}]})).unwrap();
        assert_eq!(
            windows
                .iter()
                .map(|w| (w.id.as_str(), w.label.as_str()))
                .collect::<Vec<_>>(),
            [
                ("seven_day", "Weekly"),
                ("seven_day_model_fable", "Weekly · Fable")
            ]
        );
        crate::tests::validate_http_contract(
            "accountUsage",
            &json!({"agentFamily":"claude", "checkedAtMs":1, "planType":"max", "windows": windows}),
        );
    }

    #[test]
    fn overage_is_provider_scoped_and_unknown_is_not_available() {
        assert_eq!(
            additional_usage_available(&json!({}), AgentFamily::Codex),
            None
        );
        assert_eq!(
            additional_usage_available(
                &json!({"additionalUsageAvailable":false}),
                AgentFamily::Claude
            ),
            Some(false)
        );
        let credits = |has: bool| json!({"rateLimits":{"primary":{"usedPercent":100},"credits":{"hasCredits":has,"unlimited":false}}});
        assert_eq!(
            additional_usage_available(&credits(true), AgentFamily::Codex),
            Some(true)
        );
        assert_eq!(
            additional_usage_available(&credits(false), AgentFamily::Codex),
            Some(false)
        );
        crate::tests::validate_http_contract(
            "accountUsage",
            &json!({"agentFamily":"codex","checkedAtMs":1,"windows":project(credits(true)),"additionalUsageAvailable":true}),
        );
    }

    #[test]
    fn projects_current_primary_and_secondary_windows() {
        let windows = project(json!({
            "rateLimits": {
                "primary": {"usedPercent": 12.5, "windowDurationMins": 300, "resetsAt": 10},
                "secondary": {"usedPercent": 80, "windowDurationMins": 10080, "resetsAt": 20}
            }
        }));
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "primary");
        assert_eq!(windows[0].label, "5 hours");
        assert_eq!(windows[0].remaining_percent, 87.5);
        assert_eq!(windows[1].label, "Weekly");
    }

    #[test]
    fn explicit_empty_usage_is_distinct_from_unrecognized_shape() {
        assert_eq!(
            project_windows(&json!({"rateLimits": {}})).unwrap().len(),
            0
        );
        assert_eq!(
            project_windows(&json!({"rateLimits": []})).unwrap().len(),
            0
        );
        assert!(
            project_windows(&json!({"rateLimits": {"credits": {"hasCredits": true}}})).is_none()
        );
        assert!(
            project_windows(&json!({"rateLimits": {"primary": {"usedPercent": "wrong"}}}))
                .is_none()
        );
        assert!(project_windows(&json!({"unrelated": []})).is_none());
    }

    #[test]
    fn prefers_codex_limit_id_over_generic_limits() {
        let windows = project(json!({
            "rateLimits": {"primary": {"usedPercent": 90}},
            "rateLimitsByLimitId": {
                "codex": {"primary": {"usedPercent": 25, "windowDurationMins": 60}}
            }
        }));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_percent, 25.0);
        assert_eq!(windows[0].label, "Hourly");
    }

    #[test]
    fn projects_legacy_array_without_exposing_runtime_names() {
        let windows = project(json!({
            "rateLimits": [{
                "limitName": "internal_codex_credits",
                "usedPercent": 5,
                "windowDurationMins": 60,
                "resetsAt": 30
            }]
        }));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "window-1");
        assert_eq!(windows[0].label, "Hourly");
        let output = serde_json::to_string(&windows).unwrap();
        assert!(!output.contains("internal_codex_credits"));
    }

    #[test]
    fn assigns_legacy_ids_after_filtering_invalid_entries() {
        let mut values = vec![
            json!({"usedPercent": "invalid"}),
            json!({"usedPercent": null}),
            json!({"usedPercent": 150}),
        ];
        values.extend((0..9).map(|index| json!({"usedPercent": index})));
        let windows = project(json!({"rateLimits": values}));
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert_eq!(windows[0].id, "window-1");
        assert_eq!(windows[7].id, "window-8");
        assert_eq!(windows[0].used_percent, 100.0);
    }

    #[test]
    fn falls_back_when_codex_limits_have_no_valid_windows() {
        let windows = project(json!({
            "rateLimits": {"primary": {"usedPercent": 40, "windowDurationMins": 300}},
            "rateLimitsByLimitId": {
                "codex": {"primary": {"usedPercent": "invalid"}, "secondary": {}}
            }
        }));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "primary");
        assert_eq!(windows[0].used_percent, 40.0);
    }

    #[test]
    fn clamps_percentages_and_omits_invalid_windows() {
        let windows = project(json!({
            "rateLimits": [
                {"usedPercent": -20},
                {"usedPercent": 140},
                {"usedPercent": "not-a-number"},
                {"usedPercent": null},
                {"usedPercent": 50}
            ]
        }));
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].used_percent, 0.0);
        assert_eq!(windows[0].remaining_percent, 100.0);
        assert_eq!(windows[1].used_percent, 100.0);
        assert_eq!(windows[1].remaining_percent, 0.0);
        assert_eq!(windows[2].used_percent, 50.0);
    }

    #[test]
    fn accepts_explicit_empty_but_rejects_malformed_limits() {
        assert!(project_windows(&json!({"rateLimits": []}))
            .unwrap()
            .is_empty());
        assert!(project_windows(&json!({"rateLimits": {"other": {}}})).is_none());
        assert!(project_windows(&json!({"rateLimits": "unavailable"})).is_none());
        assert!(project_windows(&json!({})).is_none());
    }

    #[test]
    fn bounds_the_projected_window_count() {
        let values = (0..12)
            .map(|index| json!({"usedPercent": index}))
            .collect::<Vec<_>>();
        let windows = project(json!({"rateLimits": values}));
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert_eq!(windows[7].id, "window-8");
    }
}
