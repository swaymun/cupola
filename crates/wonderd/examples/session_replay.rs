//! Offline acceptance for the synthetic process through the shipped transport
//! and durable inbox. Does not start the daemon, a model, or an owner database.
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, time::Duration};
use wonder_app_server::{AppServerClient, BridgeLaunchConfig, LaunchConfig, RpcClient};
use wonder_store::Store;

type Error = Box<dyn std::error::Error>;

async fn request(rpc: &RpcClient, method: &str, params: Value) -> Result<Value, Error> {
    let response = rpc.request(method, params).await?;
    if let Some(error) = response.error {
        return Err(format!("{method}: {}", error.message).into());
    }
    response
        .result
        .ok_or_else(|| format!("{method}: no result").into())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let launcher = PathBuf::from(
        args.next()
            .ok_or("usage: session_replay LAUNCHER PROVIDER PYTHON")?,
    );
    let provider = args.next().ok_or("provider required")?;
    let python = PathBuf::from(args.next().ok_or("Python path required")?);
    let config: Value =
        serde_json::from_slice(&std::fs::read(launcher.with_file_name("config.json"))?)?;
    // An accelerated check proves ordering and transport, never token latency.
    if config["speed"].as_f64().unwrap_or(1.) < 1000. {
        return Err(
            "Use a separate accelerated fixture (speed >= 1000) for this bounded check".into(),
        );
    }
    let temp = tempfile::tempdir()?;
    let store = Store::connect(&format!(
        "sqlite://{}?mode=rwc",
        temp.path().join("inbox.sqlite3").display()
    ))
    .await?;
    let sink = wonderd::ingestion::notification_sink(store.clone());
    let mut client = match provider.as_str() {
        "codex" => {
            AppServerClient::spawn_with_notification_sink(
                LaunchConfig {
                    codex_bin: launcher.clone(),
                    wonder_version: "synthetic-replay-check".into(),
                    codex_home: None,
                },
                sink,
            )
            .await?
        }
        "claude" => {
            AppServerClient::spawn_bridge(
                BridgeLaunchConfig {
                    family: "claude",
                    node_bin: python,
                    entrypoint: launcher.clone(),
                    state_dir: temp.path().join("bridge"),
                    npm_cli: None,
                    claude_config_dir: None,
                    wonder_version: "synthetic-replay-check".into(),
                },
                sink,
            )
            .await?
        }
        _ => return Err("provider must be codex or claude".into()),
    };
    let rpc = client.rpc();
    let thread = config["threadId"]
        .as_str()
        .ok_or("missing fixture thread ID")?;
    let mut cursor = Value::Null;
    let mut turns = Vec::new();
    let mut page_count = 0;
    loop {
        let page = request(
            &rpc,
            "thread/turns/list",
            json!({"threadId": thread, "cursor": cursor,
            "limit": 37, "sortDirection": "desc", "itemsView": "notLoaded"}),
        )
        .await?;
        page_count += 1;
        turns.extend(
            page["data"]
                .as_array()
                .ok_or("missing turns")?
                .iter()
                .cloned(),
        );
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        if page_count > 30 {
            return Err("turn paging did not terminate".into());
        }
    }
    let expected = config["historyTurns"]
        .as_u64()
        .ok_or("prepare with --history-turns")?;
    if expected < 200 || turns.len() as u64 != expected {
        return Err("hundreds-of-turn paging was incomplete".into());
    }
    let mut unique = std::collections::HashSet::new();
    if !turns
        .iter()
        .all(|t| unique.insert(t["id"].as_str().unwrap_or("")))
    {
        return Err("duplicate turn page".into());
    }
    let mut items = 0;
    let mut item_pages = 0;
    cursor = Value::Null;
    loop {
        let page = request(
            &rpc,
            "thread/items/list",
            json!({"threadId": thread, "cursor": cursor,
            "limit": 200, "sortDirection": "asc"}),
        )
        .await?;
        items += page["data"].as_array().ok_or("missing items")?.len();
        item_pages += 1;
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        if item_pages > 100 {
            return Err("item paging did not terminate".into());
        }
    }
    let expected_items: u64 = turns
        .iter()
        .map(|t| t["itemCount"].as_u64().unwrap_or(0))
        .sum();
    if items as u64 != expected_items {
        return Err("item count differs from turn metadata".into());
    }
    let child = request(
        &rpc,
        "thread/read",
        json!({"threadId": config["childThreadId"], "includeTurns": true}),
    )
    .await?;
    if child["thread"]["parentThreadId"] != thread
        || child["thread"]["turns"][0]["items"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        return Err("child ownership/history missing".into());
    }
    let start = request(
        &rpc,
        "turn/start",
        json!({"threadId": thread,
        "input": [{"type": "text", "text": "Replay the next recorded turn."}]}),
    )
    .await?;
    let turn_id = start["turn"]["id"].clone();
    let mut notifications = 0;
    let mut approvals = 0;
    let mut deltas: HashMap<String, String> = HashMap::new();
    let mut kinds = std::collections::BTreeSet::new();
    let final_turn = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let Some((id, event)) = store.next_notification().await? else {
                tokio::time::sleep(Duration::from_millis(2)).await;
                continue;
            };
            if event["_wonderRuntimeId"] != rpc.health().id() {
                return Err::<Value, Error>("runtime provenance missing".into());
            }
            notifications += 1;
            let params = &event["params"];
            match event["method"].as_str() {
                Some(method) if method.ends_with("/requestApproval") => {
                    approvals += 1;
                    rpc.respond_value(
                        event["id"].clone(),
                        Some(json!({"decision":"accept"})),
                        None,
                    )
                    .await?;
                }
                Some("item/agentMessage/delta") => deltas
                    .entry(params["itemId"].as_str().ok_or("missing item")?.into())
                    .or_default()
                    .push_str(params["delta"].as_str().ok_or("missing delta")?),
                Some("item/completed") => {
                    let item = &params["item"];
                    kinds.insert(item["type"].as_str().ok_or("missing kind")?.to_string());
                    if item["type"] == "agentMessage"
                        && deltas.get(item["id"].as_str().unwrap_or(""))
                            != item["text"].as_str().map(|s| s.to_string()).as_ref()
                    {
                        return Err("durable deltas differ from terminal text".into());
                    }
                }
                Some("turn/completed") => {
                    if params["turn"]["id"] != turn_id {
                        return Err("wrong terminal turn".into());
                    }
                    store.acknowledge_notification(id).await?;
                    break Ok(params["turn"].clone());
                }
                _ => {}
            }
            store.acknowledge_notification(id).await?;
        }
    })
    .await??;
    if approvals == 0 || deltas.is_empty() || final_turn["status"] != "completed" {
        return Err("recorded coverage turn did not settle with approvals and streaming".into());
    }
    let mut required = vec![
        "userMessage",
        "agentMessage",
        "commandExecution",
        "fileChange",
        "mcpToolCall",
        "plan",
    ];
    if provider == "codex" {
        required.extend(["collabAgentToolCall", "imageView"]);
    } else {
        required.push("subAgentActivity");
    }
    if required.iter().any(|kind| !kinds.contains(*kind)) {
        return Err("durable inbox lost an authored coverage item kind".into());
    }
    let history = request(
        &rpc,
        "thread/read",
        json!({"threadId": thread, "includeTurns": true}),
    )
    .await?;
    if history["thread"]["turns"].as_array().and_then(|t| t.last()) != Some(&final_turn) {
        return Err("settled history differs from durable replay".into());
    }
    drop(rpc);
    client.shutdown().await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"provider": provider, "historyTurns": turns.len(),
        "turnPages": page_count, "historyItems": items, "itemPages": item_pages,
        "durableNotifications": notifications, "approvalsAnswered": approvals, "streamedItems": deltas.len(),
        "itemKinds": kinds, "runtimeTransportAndInbox": "passed", "projectHttpAndNativeRendering": "not checked",
        "speed": config["speed"], "modelCalls": 0}))?
    );
    Ok(())
}
