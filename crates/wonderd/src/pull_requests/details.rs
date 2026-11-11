//! Bounded projection of GitHub's native read fields. Do not derive
//! branch-rule requirements from check outcomes or from `mergeable` alone.
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Details {
    pub checks: Option<Vec<Check>>,
    review_decision: Option<String>,
    review_requests: Option<Vec<Reviewer>>,
    merge_state_status: Option<String>,
    mergeable: Option<String>,
    changed_files: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Check {
    name: String,
    workflow: Option<String>,
    /// `passing` for a neutral or skipped result too: GitHub lets those
    /// satisfy a required check, so they never block a merge.
    pub state: &'static str,
    result: String,
    /// Finished neutral or skipped; counted apart from passed checks.
    #[serde(skip)]
    pub neutral: bool,
    /// The check's page on GitHub or its CI provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Reviewer {
    name: String,
    kind: &'static str,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(300).collect())
}

impl Details {
    pub(super) fn read(value: &Value) -> Self {
        Self {
            checks: read_checks(value.get("statusCheckRollup")),
            review_decision: text(value, "reviewDecision"),
            review_requests: value
                .get("reviewRequests")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .take(100)
                        .map(|row| {
                            let kind = match row.get("__typename").and_then(Value::as_str) {
                                Some("Team") => "team",
                                Some("User" | "Bot") => "user",
                                _ => "unknown",
                            };
                            let name = if kind == "team" {
                                text(row, "slug").or_else(|| text(row, "name"))
                            } else {
                                text(row, "login")
                            };
                            Reviewer {
                                name: name.unwrap_or_else(|| "Unknown reviewer".into()),
                                kind,
                            }
                        })
                        .collect()
                }),
            merge_state_status: text(value, "mergeStateStatus"),
            mergeable: text(value, "mergeable"),
            changed_files: value.get("changedFiles").and_then(Value::as_u64),
        }
    }
}

/// An https page address without credentials or whitespace, bounded.
fn link(value: Option<&Value>) -> Option<String> {
    let url = value?.as_str()?;
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (url.len() <= 500
        && !host.is_empty()
        && !host.contains('@')
        && url.bytes().all(|b| b.is_ascii_graphic()))
    .then(|| url.to_owned())
}

pub(super) fn read_checks(rollup: Option<&Value>) -> Option<Vec<Check>> {
    rollup.and_then(Value::as_array).map(|rows| {
        rows.iter()
            .take(100)
            .map(|row| {
                let name = text(row, "name")
                    .or_else(|| text(row, "context"))
                    .unwrap_or_else(|| "Unnamed check".into());
                let result = match text(row, "status") {
                    Some(status) if status == "COMPLETED" => text(row, "conclusion"),
                    Some(status) => Some(status),
                    None => text(row, "state"),
                }
                .unwrap_or_else(|| "UNKNOWN".into());
                let state = match result.as_str() {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => "passing",
                    "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                    | "STALE" | "STARTUP_FAILURE" => "failing",
                    "QUEUED" | "IN_PROGRESS" | "REQUESTED" | "WAITING" | "PENDING" | "EXPECTED" => {
                        "pending"
                    }
                    _ => "unknown",
                };
                Check {
                    name,
                    workflow: text(row, "workflowName"),
                    state,
                    neutral: matches!(result.as_str(), "NEUTRAL" | "SKIPPED"),
                    result,
                    url: link(row.get("detailsUrl").or_else(|| row.get("targetUrl"))),
                }
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::super::{checks, pull_request};
    use super::*;
    use serde_json::json;

    // Native fields must survive projection; a missing or new check result
    // cannot become a passing check or an invented merge blocker.
    #[test]
    fn native_pr_details_preserve_reviewers_checks_and_unknowns() {
        let row = json!({"number":5,"title":"Review me","state":"OPEN","isDraft":false,
            "url":"https://github.com/o/r/pull/5", "changedFiles":7,
            "mergeStateStatus":"BLOCKED", "mergeable":"MERGEABLE", "reviewDecision":"REVIEW_REQUIRED",
            "reviewRequests":[{"__typename":"User","login":"alex"},
                {"__typename":"Team","slug":"o/mobile","name":"Mobile"}],
            "statusCheckRollup":[{"name":"Unit tests","workflowName":"CI","status":"COMPLETED","conclusion":"SUCCESS"},
                {"context":"External check","state":"FUTURE_STATE"}]});
        let projected = serde_json::to_value(pull_request(&row).unwrap()).unwrap();
        assert_eq!(projected["details"]["changedFiles"], 7);
        assert_eq!(projected["details"]["checks"][0]["workflow"], "CI");
        assert_eq!(
            projected["details"]["reviewRequests"][1]["name"],
            "o/mobile"
        );
        assert_eq!(projected["details"]["mergeStateStatus"], "BLOCKED");
        assert_eq!(projected["details"]["checks"][1]["result"], "FUTURE_STATE");
        assert_eq!(projected["checks"]["state"], "unknown");
        assert_eq!(projected["checks"]["unknown"], 1);
        for (input, expected) in [
            (json!({"status":"COMPLETED","conclusion":null}), "unknown"),
            (
                json!({"status":"COMPLETED","conclusion":"FUTURE"}),
                "unknown",
            ),
            (json!({"state":"SUCCESS"}), "passing"),
            (
                json!({"status":"COMPLETED","conclusion":"SKIPPED"}),
                "passing",
            ),
            (
                json!({"status":"COMPLETED","conclusion":"CANCELLED"}),
                "failing",
            ),
        ] {
            assert_eq!(checks(Some(&json!([input]))).state, expected);
        }
        let page = json!(vec![json!({"state":"SUCCESS"}); 100]);
        assert_eq!(
            checks(Some(&page)).state,
            "unknown",
            "a full native page cannot establish all checks passed"
        );
        assert!(checks(Some(&page)).limited);
        let mut failing_page = page;
        failing_page[0] = json!({"state":"FAILURE"});
        let partial = checks(Some(&failing_page));
        assert_eq!(partial.state, "failing");
        assert!(partial.limited);
        let missing = serde_json::to_value(Details::read(&json!({}))).unwrap();
        assert!(missing["reviewRequests"].is_null());
        assert!(missing["checks"].is_null());
        assert!(missing["changedFiles"].is_null());
    }
}
