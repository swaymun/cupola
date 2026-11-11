//! OpenCode and Cursor threads T3 Code runs have no session Cupola can attach
//! (`native_sessions` is false for both), so their chat reads T3 Code's own
//! timeline (`t3_thread_read`, view `activity`) instead of a provider runtime.
//! The conversation's runtime thread is the synthetic `opencode-t3:<thread>` /
//! `cursor-t3:<thread>`; its history refresh pages the T3 thread and commits
//! the same `{turnId, item}` entries a runtime would list. Item shapes not
//! recorded from T3 Code (anything but messages, reasoning, commands and
//! errors) are left out rather than guessed.
use super::*;

const PAGE: i64 = 100;
const MAX_PAGES: u32 = 200;

/// The runtime thread ID Cupola keeps for a T3-native conversation.
pub(crate) fn runtime_id(family: AgentFamily, thread: &str) -> Option<String> {
    let prefix = family.provider().thread_id_prefix?;
    Some(format!("{prefix}t3:{thread}"))
}

/// The T3 thread a runtime thread ID stands for, when it is synthetic.
pub(crate) fn t3_thread(runtime_thread: &str) -> Option<&str> {
    ["opencode-t3:", "cursor-t3:"]
        .into_iter()
        .find_map(|prefix| runtime_thread.strip_prefix(prefix))
        .filter(|thread| !thread.is_empty())
}

/// A durable T3-only runtime must never be resumed by a provider, even
/// when T3 is disconnected or its ownership record has been removed.
pub(crate) async fn bound_thread(
    state: &AppState,
    stored: &StoredProjectConversation,
) -> Option<String> {
    if !matches!(stored.family, AgentFamily::OpenCode | AgentFamily::Cursor) {
        return None;
    }
    let binding = state
        .store
        .runtime_binding(&stored.conversation_id)
        .await
        .ok()??;
    let thread = t3_thread(&binding.thread_id)?;
    (binding.family == stored.family
        && binding.execution_scope == wonder_store::EXECUTION_SCOPE_PROJECTS
        && runtime_id(stored.family, thread).as_deref() == Some(binding.thread_id.as_str()))
    .then(|| thread.to_owned())
}

/// T3 Code's timeline as `(turnId, item)` entries in the shape a runtime's
/// `thread/items/list` returns. A turn opens at each user message.
pub(crate) fn entries(items: &[Value]) -> Vec<Value> {
    entries_except(items, &[])
}

/// As `entries`, leaving out T3 Code's record of messages Cupola sent
/// (`message:mcp:<grant>:thread-send:<message>` for each `sent` message
/// ID): Cupola's own message already shows, with its delivery state, so
/// the phone would show it twice. The record still opens its turn.
pub(crate) fn entries_except(items: &[Value], sent: &[String]) -> Vec<Value> {
    let echo = |item: &Value| {
        item["messageId"].as_str().is_some_and(|id| {
            id.starts_with("message:mcp:")
                && sent.iter().any(|message| {
                    id.ends_with(&format!(":thread-send:{}", super::uri_component(message)))
                })
        })
    };
    let mut turn = "t3-turn-0".to_owned();
    let mut out: Vec<Value> = Vec::new();
    // Whether the last entry is an assistant message, which the next
    // assistant message continues (Cursor sends one item per chunk).
    let mut open_reply = false;
    for item in items {
        let kind = item["type"].as_str().unwrap_or_default();
        let Some(id) = item["itemId"].as_str().or_else(|| item["id"].as_str()) else {
            continue;
        };
        let text = item["text"].as_str().unwrap_or_default();
        let status = match item["status"].as_str() {
            Some("failed") => "failed",
            Some("running" | "in_progress" | "streaming") => "inProgress",
            _ => "completed",
        };
        if kind == "user_message" {
            turn = format!("t3-turn-{}", item["position"].as_i64().unwrap_or_default());
            if echo(item) {
                open_reply = false;
                continue;
            }
        }
        // Chunks of one reply join; a reply from another provider never does.
        let provider = item_provider(id);
        if kind == "assistant_message" && open_reply {
            if let Some(last) = out
                .last_mut()
                .filter(|last| last["item"]["t3Provider"].as_str() == provider)
            {
                let joined = format!(
                    "{}\n\n{text}",
                    last["item"]["text"].as_str().unwrap_or_default()
                );
                last["item"]["text"] = joined.into();
                last["item"]["status"] = status.into();
                continue;
            }
        }
        open_reply = kind == "assistant_message";
        let mut mapped = match kind {
            "user_message" => json!({"id": id, "type": "userMessage", "status": status,
                "content": [{"type": "text", "text": text}]}),
            "assistant_message" => {
                json!({"id": id, "type": "agentMessage", "status": status, "text": text})
            }
            "reasoning" => json!({"id": id, "type": "reasoning", "status": status,
                "summary": [text], "content": []}),
            "command_execution" => {
                let body = text.strip_prefix("$ ").unwrap_or(text);
                let (command, output) = body.split_once('\n').unwrap_or((body, ""));
                json!({"id": id, "type": "commandExecution", "status": status,
                    "command": command, "aggregatedOutput": output, "commandActions": []})
            }
            // The provider's own words, plainly: no error item type exists.
            "error" => {
                let title = item["title"].as_str().unwrap_or("Provider error");
                json!({"id": id, "type": "agentMessage", "status": "failed",
                    "text": if text.is_empty() { title.to_owned() } else { format!("{title}: {text}") }})
            }
            _ => continue,
        };
        // Preserve T3's explicit order when ingestion timestamps coincide.
        // Refresh time is not the order of the provider's commands/replies.
        if let Some(position) = item["position"].as_i64() {
            mapped["t3Position"] = position.into();
        }
        // The time T3 Code recorded, so a thread read late keeps it. Items
        // without one fall back to the time Cupola first saved them.
        if let Some(at) = t3_recorded_time(item) {
            mapped["createdAt"] = at.into();
        }
        // Which provider produced the item, as T3 Code recorded it. Items
        // without that record (user messages, notifications) carry none.
        if kind != "user_message" {
            if let Some(provider) = provider {
                mapped["t3Provider"] = provider.into();
            }
        }
        out.push(json!({"turnId": turn, "item": mapped}));
    }
    out
}

/// The RFC 3339 time T3 Code recorded on a timeline item, if it sent one.
fn t3_recorded_time(item: &Value) -> Option<&str> {
    ["updatedAt"]
        .iter()
        .filter_map(|key| item[*key].as_str())
        .find(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
}

/// The provider T3 Code recorded in a timeline item's ID
/// (`turn-item:provider:<provider>:native-item:...`), e.g. `claudeAgent`.
pub(crate) fn item_provider(item_id: &str) -> Option<&str> {
    let rest = item_id.strip_prefix("turn-item:provider:")?;
    let (provider, _) = rest.split_once(':')?;
    (!provider.is_empty()).then_some(provider)
}

impl T3Code {
    /// Every item of the T3 thread's activity timeline, oldest first.
    pub(crate) async fn activity_items(&self, thread: &str) -> Result<Vec<Value>, CallError> {
        let mut items = Vec::new();
        let mut after: Option<i64> = None;
        for _ in 0..MAX_PAGES {
            let mut arguments = json!({"threadId": thread, "view": "activity", "limit": PAGE});
            if let Some(after) = after {
                arguments["afterPosition"] = after.into();
            }
            let page = self.call("t3_thread_read", arguments).await?;
            let got = page["items"].as_array().cloned().unwrap_or_default();
            let next = page["nextPosition"].as_i64().or_else(|| {
                got.iter()
                    .filter_map(|item| item["position"].as_i64())
                    .max()
            });
            let more = page["hasMore"].as_bool() == Some(true) && !got.is_empty();
            items.extend(got);
            match (more, next) {
                (true, Some(next)) if after != Some(next) => after = Some(next),
                _ => break,
            }
        }
        Ok(items)
    }
}

/// Activity uses the committed chat page plus T3's run state; a synthetic
/// runtime has no provider RPC to poll. The stream refreshes its saved items.
pub(crate) async fn activity(
    state: &AppState,
    conversation: &str,
    thread: &str,
) -> Result<Value, String> {
    let page = state
        .store
        .conversation_history_page(&state.host_epoch, conversation, None, 16)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Chat history unavailable")?;
    let projection = crate::history::conversation_thread_projection_with_items(
        page.codex_thread_id,
        &page.messages,
        &page.assistant_messages,
        &page.events,
        &[],
        Some(&page.workspace_path),
    );
    let latest = projection
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.id.starts_with("local:"));
    let running = state
        .t3_code
        .active_run(thread)
        .await
        .map_err(|e| format!("{e:?}"))?
        .is_some();
    super::questions::watch(state, conversation, thread);
    super::stream::watch(state, conversation, thread);
    let status = if running {
        "inProgress"
    } else if latest.is_some_and(|turn| turn.items.iter().any(|item| item.state == "failed")) {
        "failed"
    } else if latest.is_some() {
        "completed"
    } else {
        "none"
    };
    let (count, newest) = state
        .store
        .conversation_history_mark(conversation)
        .await
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"latestTurnId": latest.map(|turn| &turn.id), "latestTurnStatus": status,
        "latestTurnItems": latest.map(|turn| turn.items.len()), "historyRevision": format!("t3:{count}:{newest}"),
        "threadStatus": {"type": if running { "active" } else { "idle" }},
        "runningElsewhere": running, "openElsewhere": false, "tasksOnMac": false,
        "sendStopsMacReply": false, "runsWithoutT3Code": false}),
    )
}

/// How a turn ended, from its last item: an error or failed final item is a
/// failed turn (a command that failed on the way is not).
fn turn_status(entries: &[Value], turn: &str) -> &'static str {
    let last = entries
        .iter()
        .rev()
        .find(|entry| entry["turnId"].as_str() == Some(turn));
    match last.and_then(|entry| entry["item"]["status"].as_str()) {
        Some("failed") => "failed",
        _ => "completed",
    }
}

/// Saves the T3 thread's timeline into the conversation (`history`'s
/// refresh for a synthetic runtime thread).
pub(crate) async fn refresh(
    state: &AppState,
    conversation: &str,
    runtime_thread: &str,
    t3_thread: &str,
    token: &str,
) -> Result<(), String> {
    let items = state
        .t3_code
        .activity_items(t3_thread)
        .await
        .map_err(|e| format!("T3 Code history unavailable: {e:?}"))?;
    let workspace = state
        .store
        .conversation_execution_directory(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let sent: Vec<String> = state
        .store
        .t3_code_sent_messages(conversation)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|send| send.id)
        .collect();
    let mut page = Vec::new();
    let shown = entries_except(&items, &sent);
    // T3 Code sends no turn lifecycle, so a turn's outcome is read from its
    // items; without one a reply with no tool work reads "Work status
    // unavailable". The newest turn gets none while T3 Code runs it; a run
    // state that can't be read counts as idle, never running.
    let running = state.t3_code.run_in_progress(t3_thread).await;
    let last_turn = shown.last().and_then(|entry| entry["turnId"].as_str());
    let observed_at = chrono::Utc::now().timestamp_millis();
    for entry in &shown {
        let turn = entry["turnId"].as_str().unwrap_or_default();
        let item = &entry["item"];
        let (item_state, _) = crate::history::history_item_lifecycle(entry, item);
        let detail = crate::thread_item_upsert_detail(turn, item, item_state, workspace.as_deref())
            .ok_or("Missing history item identity")?;
        let mut detail: Value = serde_json::from_str(&detail).map_err(|e| e.to_string())?;
        detail["historyRefresh"] = Value::Bool(true);
        if !(running && Some(turn) == last_turn) {
            detail["turnStatus"] = turn_status(&shown, turn).into();
            detail["turnStatusObservedAtMs"] = observed_at.into();
        }
        page.push(crate::event_envelope(
            state,
            crate::WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(detail.to_string()),
            },
            crate::EventContext {
                conversation_id: Some(conversation.into()),
                thread_id: Some(runtime_thread.into()),
                turn_id: Some(turn.into()),
                item_id: item["id"].as_str().map(str::to_owned),
                ..Default::default()
            },
        ));
    }
    let _dispatch = state.dispatch_lock.lock().await;
    if !state
        .store
        .history_refresh_is_current(conversation, token)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("Refresh superseded".into());
    }
    for envelope in state
        .store
        .commit_history_refresh(page)
        .await
        .map_err(|e| e.to_string())?
    {
        let _ = state.events.send(envelope);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T3 Code's activity items as recorded 2026-10-31 (Cursor grok-4.7 and
    /// OpenCode beam-free, T3 Code `t3_thread_read`).
    fn cursor() -> Vec<Value> {
        vec![
            json!({"position":0,"type":"user_message","status":"completed","itemId":"turn-item:message:message%3Adelegated-task%3A1","text":"run ls"}),
            json!({"position":1,"type":"reasoning","status":"completed","itemId":"turn-item:provider:cursor:native-item:reasoning%3Arun-1%3A1","text":"The user wants"}),
            json!({"position":2,"type":"assistant_message","status":"completed","itemId":"turn-item:provider:cursor:native-item:assistant%3Arun-1%3A1","text":"I'll run those."}),
            json!({"position":3,"type":"command_execution","status":"completed","itemId":"c0","text":"$ ls plans | head -3\na.md\nb.md\n"}),
            json!({"position":4,"type":"command_execution","status":"completed","itemId":"c1","text":"$ cat /definitely-missing-file.txt\ncat: /definitely-missing-file.txt: No such file or directory\n"}),
            json!({"position":5,"type":"checkpoint","status":"completed","itemId":"k","text":"[]"}),
        ]
    }

    #[test]
    fn items_carry_the_provider_t3_recorded_and_pass_through_the_projection() {
        let items = vec![
            json!({"position":0,"type":"user_message","status":"completed","itemId":"turn-item:message:m1","text":"go"}),
            json!({"position":1,"type":"assistant_message","status":"completed","itemId":"turn-item:provider:claudeAgent:native-item:a%3A1","text":"Claude says"}),
            json!({"position":2,"type":"user_message","status":"completed","itemId":"turn-item:message:m2","text":"again"}),
            json!({"position":3,"type":"assistant_message","status":"completed","itemId":"turn-item:provider:opencode:native-item:a%3A2","text":"OpenCode says"}),
            json!({"position":4,"type":"assistant_message","status":"completed","itemId":"plain","text":"unknown"}),
            json!({"position":5,"type":"assistant_message","status":"completed","itemId":"turn-item:provider:claudeAgent:native-item:a%3A3","text":"Claude again"}),
        ];
        let entries = entries(&items);
        let providers: Vec<_> = entries
            .iter()
            .map(|e| e["item"]["t3Provider"].clone())
            .collect();
        assert_eq!(
            providers,
            [
                Value::Null,
                "claudeAgent".into(),
                Value::Null,
                "opencode".into(),
                Value::Null,
                "claudeAgent".into()
            ]
        );
        let shown = crate::history::typed_thread_item_projection(
            &crate::history::AppServerThreadItem {
                turn_id: "t".into(),
                item: entries[3]["item"].clone(),
            },
            "1970-01-01T00:00:00Z",
            None,
        );
        assert_eq!(shown.payload["t3Provider"], "opencode");
        assert_eq!(
            item_provider("turn-item:provider:cursor:native-item:x"),
            Some("cursor")
        );
        assert_eq!(item_provider("turn-item:message:x"), None);
    }

    #[test]
    fn items_keep_the_time_t3_recorded() {
        let mut items = cursor();
        items[2]["updatedAt"] = "2026-11-08T22:07:00Z".into();
        items[3]["updatedAt"] = "not a time".into();
        let mapped = entries(&items);
        assert_eq!(mapped[2]["item"]["createdAt"], "2026-11-08T22:07:00Z");
        assert!(mapped[3]["item"].get("createdAt").is_none());
    }

    #[test]
    fn cursor_timeline_maps_to_one_turn_of_items() {
        let entries = entries(&cursor());
        assert_eq!(entries.len(), 5, "checkpoint is not a chat item");
        assert!(entries.iter().all(|e| e["turnId"] == "t3-turn-0"));
        let kinds: Vec<_> = entries.iter().map(|e| e["item"]["type"].clone()).collect();
        assert_eq!(
            kinds,
            [
                "userMessage",
                "reasoning",
                "agentMessage",
                "commandExecution",
                "commandExecution"
            ]
        );
        let failing = &entries[4]["item"];
        assert_eq!(failing["command"], "cat /definitely-missing-file.txt");
        assert!(failing["aggregatedOutput"]
            .as_str()
            .unwrap()
            .contains("No such file"));
        assert_eq!(turn_status(&entries, "t3-turn-0"), "completed");
    }

    #[test]
    fn adjacent_assistant_chunks_are_one_reply() {
        let entries = entries(&[
            json!({"position":0,"type":"user_message","itemId":"u","text":"hi"}),
            json!({"position":1,"type":"assistant_message","itemId":"a1","text":"One."}),
            json!({"position":2,"type":"assistant_message","itemId":"a2","text":"Two."}),
            json!({"position":3,"type":"command_execution","itemId":"c","text":"$ ls\n"}),
            json!({"position":4,"type":"assistant_message","itemId":"a3","text":"Done."}),
        ]);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1]["item"]["text"], "One.\n\nTwo.");
        assert_eq!(entries[3]["item"]["text"], "Done.");
    }

    #[test]
    fn opencode_failed_turn_shows_the_provider_error_plainly() {
        let entries = entries(&[
            json!({"position":0,"type":"user_message","status":"completed","itemId":"u","text":"hi"}),
            json!({"position":1,"type":"error","status":"failed","title":"Provider error","itemId":"e",
                "text":"Upstream request failed: [invalid_value] Invalid value for 'tools': maximum number of items is 128."}),
        ]);
        assert_eq!(entries[1]["item"]["status"], "failed");
        assert_eq!(
            entries[1]["item"]["text"],
            "Provider error: Upstream request failed: [invalid_value] Invalid value for 'tools': maximum number of items is 128."
        );
        assert_eq!(entries[1]["turnId"], "t3-turn-0");
        assert_eq!(turn_status(&entries, "t3-turn-0"), "failed");
    }

    #[test]
    fn cupola_sent_messages_show_once_and_keep_their_turn() {
        let items = [
            json!({"position":0,"type":"user_message","itemId":"u0","messageId":"mcp:t","text":"hi"}),
            json!({"position":1,"type":"assistant_message","itemId":"a1","text":"pong"}),
            json!({"position":2,"type":"user_message","itemId":"u2",
                "messageId":"message:mcp:client%3Ag:thread-send:m-1","text":"again"}),
            json!({"position":3,"type":"assistant_message","itemId":"a3","text":"ping"}),
            json!({"position":4,"type":"user_message","itemId":"u4",
                "messageId":"message:mcp:client%3Aother:thread-send:typed-elsewhere","text":"mine"}),
        ];
        let shown = entries_except(&items, &["m-1".to_owned()]);
        let ids: Vec<_> = shown
            .iter()
            .map(|e| e["item"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["u0", "a1", "a3", "u4"]);
        assert_eq!(
            shown[2]["turnId"], "t3-turn-2",
            "the reply stays in the turn Cupola's message opened"
        );
    }

    /// A T3 user message carries its mention markup and `<t3_context>`
    /// block; the feed's item shows the owner's words through the same
    /// projection every history item uses (`typed_item_display_text`).
    #[test]
    fn t3_mention_markup_in_a_feed_user_message_shows_the_owners_words() {
        let items = [json!({"position":0,"type":"user_message","itemId":"u0",
            "text":"look at [Thread: Plan; ref=thread_1] please\n\n<t3_context version=\"1\">\n<context kind=\"thread\" id=\"thread_1\">\ntitle: Plan\n</context>\n</t3_context>"})];
        let shown = entries(&items);
        assert_eq!(
            crate::history::typed_item_display_text(&shown[0]["item"]).as_deref(),
            Some("look at Thread: Plan please")
        );
    }

    #[test]
    fn runtime_ids_round_trip_and_stay_per_provider() {
        let id = runtime_id(AgentFamily::OpenCode, "thread:x").unwrap();
        assert_eq!(id, "opencode-t3:thread:x");
        assert_eq!(t3_thread(&id), Some("thread:x"));
        assert!(AgentFamily::OpenCode.accepts_thread_id(&id));
        assert_eq!(t3_thread("opencode-ses_1"), None);
        assert_eq!(
            t3_thread(&runtime_id(AgentFamily::Cursor, "t").unwrap()),
            Some("t")
        );
    }
}
