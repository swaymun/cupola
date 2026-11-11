//! Native task updates are snapshots, not proposed-plan markdown.
use serde_json::{json, Value};

pub(super) fn snapshot(entries: &Value, text_key: &str) -> Option<Value> {
    let entries = entries.as_array()?;
    let completed = entries
        .iter()
        .filter(|entry| entry["status"] == "completed")
        .count();
    let mut truncated = entries.len() > 32;
    let rows = entries.iter().take(32).map(|entry| {
        let text = entry[text_key].as_str().unwrap_or("");
        truncated |= text.chars().count() > 256;
        let status = entry["status"].as_str().unwrap_or("unknown");
        json!({"text": super::sanitize_ansi_and_secrets(text, 256),
            "status": match status { "in_progress" => "inProgress", other => other }.chars().take(64).collect::<String>()})
    }).collect::<Vec<_>>();
    Some(
        json!({"entries": rows, "total": entries.len(), "completed": completed, "truncated": truncated}),
    )
}

pub(super) fn notification_item(turn: &str, params: &Value) -> Option<Value> {
    let snapshot = snapshot(&params["plan"], "step")?;
    Some(
        json!({"type":"taskChecklist", "id":format!("{turn}:tasks"), "status":"completed", "updatedAt":super::now_ms().to_string(), "taskChecklist":snapshot}),
    )
}

/// Bound both bridge-authored lists and provider TodoWrite results before storage.
pub(super) fn sanitize(item: &mut Value) {
    let Some(raw) = item.get("taskChecklist").cloned() else {
        return;
    };
    let Some(mut latest) = snapshot(&raw["entries"], "text") else {
        item.as_object_mut()
            .map(|object| object.remove("taskChecklist"));
        return;
    };
    // A previously bounded snapshot retains its whole-list counts.
    for key in ["total", "completed"] {
        if let Some(count) = raw[key].as_u64().filter(|count| *count <= 1_000_000) {
            latest[key] = count.into();
        }
    }
    latest["truncated"] = json!(latest["truncated"] == true || raw["truncated"] == true);
    if let Some(history) = raw["history"].as_array() {
        latest["history"] = Value::Array(
            history
                .iter()
                .rev()
                .take(8)
                .rev()
                .filter_map(|entry| {
                    let mut copy = json!({"taskChecklist":entry});
                    // History contains snapshots only, never recursive histories.
                    copy["taskChecklist"].as_object_mut()?.remove("history");
                    sanitize(&mut copy);
                    copy.get("taskChecklist").cloned()
                })
                .collect(),
        );
    }
    item["taskChecklist"] = latest;
}

/// Runtime hydration has a richer source rank, but not necessarily a newer
/// native checklist. Reconcile these two inputs without losing journal history.
pub(super) fn merge_projection(existing: &Value, incoming: &Value) -> Option<Value> {
    let old = existing.get("taskChecklist")?;
    let new = incoming.get("taskChecklist")?;
    let stamp = |item: &Value| {
        item["updatedAt"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
    };
    let newer = match (stamp(existing), stamp(incoming)) {
        (Some(old), Some(new)) if old != new => new > old,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        _ => new.get("history").is_some(),
    };
    let (latest, earlier) = if newer {
        (incoming, old)
    } else {
        (existing, new)
    };
    let mut result = latest.clone();
    let mut snapshot = latest["taskChecklist"].clone();
    snapshot.as_object_mut()?.remove("history");
    let mut history = latest["taskChecklist"]["history"]
        .as_array()
        .or_else(|| earlier["history"].as_array())
        .cloned()
        .unwrap_or_default();
    if history.is_empty() && earlier != &snapshot {
        let mut first = earlier.clone();
        first.as_object_mut()?.remove("history");
        history.push(first);
    }
    if history.last() != Some(&snapshot) {
        history.push(snapshot);
    }
    if history.len() > 8 {
        history.drain(..history.len() - 8);
    }
    result["taskChecklist"]["history"] = Value::Array(history);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_plan_snapshot_is_not_proposed_plan_and_keeps_unknown_states() {
        let item = notification_item(
            "turn",
            &json!({"plan":[
                {"step":"Read code","status":"completed"}, {"step":"Check output","status":"future"}
            ]}),
        )
        .unwrap();
        assert_eq!(item["type"], "taskChecklist");
        assert_eq!(item["taskChecklist"]["completed"], 1);
        assert_eq!(item["taskChecklist"]["entries"][1]["status"], "future");
        let old = json!({"updatedAt":"1000","taskChecklist":{"entries":[],"total":0,"completed":0,
            "history":[{"entries":[],"total":0,"completed":0}]}});
        let new = json!({"updatedAt":"1001","taskChecklist":{"entries":[{"text":"Check output","status":"pending"}],"total":1,"completed":0}});
        for (left, right) in [(&old, &new), (&new, &old)] {
            let merged = merge_projection(left, right).unwrap();
            assert_eq!(merged["updatedAt"], "1001");
            assert_eq!(merged["taskChecklist"]["total"], 1);
            assert_eq!(
                merged["taskChecklist"]["history"].as_array().unwrap().len(),
                2
            );
        }

        let key = |stamp| {
            crate::app_server_notification_key(
                "item/completed",
                &json!({}),
                &json!({"threadId":"thread","turnId":"turn","notificationId":format!("turn:tasks:{stamp}"),
                "item":{"id":"turn:tasks","type":"taskChecklist"}}),
            )
        };
        assert_ne!(
            key(1000),
            key(1001),
            "Distinct native updates survive completed-item deduplication"
        );
        assert_eq!(
            key(1000),
            key(1000),
            "A replay still deduplicates the exact update"
        );
        assert!(notification_item("turn", &json!({"plan":"markdown"})).is_none());
        let mut large = notification_item(
            "turn",
            &json!({"plan":vec![json!({"step":"x".repeat(500),"status":"pending"});40]}),
        )
        .unwrap();
        sanitize(&mut large);
        assert_eq!(
            large["taskChecklist"]["entries"].as_array().unwrap().len(),
            32
        );
        assert_eq!(large["taskChecklist"]["total"], 40);
        assert_eq!(large["taskChecklist"]["completed"], 0);
        assert_eq!(large["taskChecklist"]["truncated"], true);
    }
}
