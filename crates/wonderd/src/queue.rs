use super::*;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    expected_revision: i64,
    body: Option<String>,
    cancel: Option<bool>,
    expected_turn_id: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Reorder {
    items: Vec<(String, i64)>,
}
/// Whether an accepted message is queued rather than being sent now: it waits
/// behind other work in its conversation, or the Project chat is held (Claude
/// or Codex on the Mac is running a turn in it, it is open in Claude Code in a
/// terminal, or Wonder's Claude runtime still runs a reply in it) and T3 Code
/// does not take it into its thread there. The one owner of that decision for
/// the send receipt and the queue list, so the phone's first placement of a
/// message is final; dispatch asks the same (`projects::send_waits`).
pub(crate) async fn waits(state: &AppState, conversation: &str, behind_work: bool) -> bool {
    behind_work
        || (crate::projects::is_project(state, conversation).await
            && crate::projects::send_waits(state, conversation, None).await)
}

/// The send receipt for an accepted message, saying whether it is queued.
pub(crate) async fn receipt(
    state: &AppState,
    message: wonder_store::StoredMessage,
) -> Option<ClientMessageReceipt> {
    let queued = match message.state.as_str() {
        "accepted_by_wonder" => {
            let behind = state
                .store
                .message_behind_work(&message.id)
                .await
                .unwrap_or(false);
            waits(state, &message.conversation_id, behind).await
        }
        _ => false,
    };
    let mut receipt = message_receipt(message)?;
    receipt.queued = Some(queued);
    Some(receipt)
}

pub(super) async fn list(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
) -> Response {
    let items = match state.store.pending_queue(&conversation).await {
        Ok(items) => items,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    // The first pending message with nothing ahead is being sent (dispatch may
    // simply not have claimed it yet) unless the Mac holds the chat.
    let held = match items.iter().any(|item| !item.behind_work) {
        true => waits(&state, &conversation, false).await,
        false => false,
    };
    let items = items
        .into_iter()
        .filter(|item| item.behind_work || held)
        .collect::<Vec<_>>();
    // Messages T3 Code holds in its queue come first: they were sent to it
    // before any of Cupola's own waiting messages (`t3_code::queue`).
    let in_t3_code = crate::t3_code::queue::queued(&state, &conversation).await;
    if !in_t3_code.is_empty() {
        crate::t3_code::queue::check_soon(&state, &conversation);
    }
    let mut result = Vec::new();
    for item in in_t3_code {
        let attachments = state
            .store
            .attachments_for_message(&item.message)
            .await
            .unwrap_or_default();
        result.push(
            serde_json::json!({"id": item.message, "clientMessageId": item.client,
            "body": item.body, "revision": item.revision,
            "attachmentIds": attachments.iter().map(|file| file.id.clone()).collect::<Vec<_>>(),
            "codexSkills": [], "waitsInT3Code": true}),
        );
    }
    for item in items {
        let value = serde_json::json!({"id":item.id,"clientMessageId":item.client_message_id,"body":item.body,"revision":item.revision,"attachmentIds":item.attachment_ids,"codexSkills":item.codex_skills});
        result.push(value);
    }
    Json(result).into_response()
}
pub(super) async fn edit(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
    Json(request): Json<Edit>,
) -> Response {
    if let Some(response) = crate::subagents::reject_user_mutation(&state, &conversation).await {
        return response;
    }
    if let Some(turn) = request.expected_turn_id {
        if request.body.is_some() || request.cancel.unwrap_or(false) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if let Some(response) = crate::t3_code::queue::change(
            &state,
            &conversation,
            &id,
            request.expected_revision,
            crate::t3_code::queue::Change::SendNow(turn.clone()),
        )
        .await
        {
            return response;
        }
        return match state
            .store
            .queue_to_guide(&conversation, &id, request.expected_revision, &turn)
            .await
        {
            Ok(true) => {
                // Steer now rather than on the dispatch loop's next tick: the
                // phone shows "Sending…" until the agent takes the message.
                state.dispatch_wake.notify_one();
                StatusCode::NO_CONTENT.into_response()
            }
            Ok(false) => (
                StatusCode::CONFLICT,
                "This message or the active work changed. Nothing was moved.",
            )
                .into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    let cancel = request.cancel.unwrap_or(false);
    if cancel == request.body.is_some()
        || request
            .body
            .as_ref()
            .is_some_and(|b| b.trim().is_empty() || b.len() > 65536)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let change = match &request.body {
        Some(body) => crate::t3_code::queue::Change::Edit(body.clone()),
        None => crate::t3_code::queue::Change::Cancel,
    };
    if let Some(response) = crate::t3_code::queue::change(
        &state,
        &conversation,
        &id,
        request.expected_revision,
        change,
    )
    .await
    {
        return response;
    }
    let hash = request
        .body
        .as_ref()
        .map(|b| hex::encode(Sha256::digest(b.as_bytes())));
    let body = request.body.as_deref().zip(hash.as_deref());
    match state
        .store
        .edit_pending(&conversation, &id, request.expected_revision, body)
        .await
    {
        Ok(true) => {
            let _ = publish_event_with_context(
                &state,
                WonderEvent::Activity {
                    category: "conversation".into(),
                    state: "updated".into(),
                    detail: None,
                },
                EventContext {
                    conversation_id: Some(conversation),
                    ..EventContext::default()
                },
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            "This message changed or started. Refresh the queue; your edit has not been applied.",
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
pub(super) async fn reorder(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(request): Json<Reorder>,
) -> Response {
    if let Some(response) = crate::subagents::reject_user_mutation(&state, &conversation).await {
        return response;
    }
    if request.items.len() > 1000 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state
        .store
        .reorder_pending(&conversation, &request.items)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::CONFLICT,
            "The queue changed. Refresh before reordering.",
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn queued_body_edit_is_revision_checked_and_cannot_change_started_work() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        crate::test_support::project_thread(
            &state.store,
            "d19b0f7e-97bb-45d7-8206-324a62386dd0",
            "Queue",
            state.data_dir.to_str().unwrap(),
            "claude-fixture",
            Some("claude:sonnet"),
            Some("high"),
            "now",
        )
        .await
        .unwrap();

        let MessageInsert::Inserted(active) = state
            .store
            .insert_message(
                "owner",
                "active",
                "work",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd0",
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("active")
        };
        state
            .store
            .update_message_delivery(&active.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                "settings",
                "work",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd0",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        let response = list(
            State(state.clone()),
            Path("d19b0f7e-97bb-45d7-8206-324a62386dd0".into()),
        )
        .await;
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let queued: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        crate::test_support::assert_http_contract("queueList", &queued);
        let request = || Edit {
            expected_revision: 1,
            body: Some("Revised work".into()),
            cancel: None,
            expected_turn_id: None,
        };
        // Editing text must keep this message's accepted model, even after the
        // composer selects another model for later messages.
        state
            .store
            .update_project_conversation(
                &message.conversation_id,
                wonder_store::ProjectConversationPatch {
                    model: Some("claude:haiku"),
                    effort: Some(None),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        assert_eq!(
            edit(
                State(state.clone()),
                Path((
                    "d19b0f7e-97bb-45d7-8206-324a62386dd0".into(),
                    message.id.clone()
                )),
                Json(request())
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .body,
            "Revised work"
        );
        assert_eq!(
            state
                .store
                .project_message_execution_settings(&message.id)
                .await
                .unwrap(),
            Some((Some("claude:sonnet".into()), Some("high".into()), None))
        );
        assert_eq!(
            edit(
                State(state.clone()),
                Path((
                    "d19b0f7e-97bb-45d7-8206-324a62386dd0".into(),
                    message.id.clone()
                )),
                Json(request())
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let mut next = request();
        next.expected_revision = 2;
        assert_eq!(
            edit(
                State(state.clone()),
                Path(("d19b0f7e-97bb-45d7-8206-324a62386dd0".into(), message.id)),
                Json(next)
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }

    // Contract (A3): a message queued while a long reply runs shows as Queued,
    // then, once it starts, in the conversation's newest history page, in its
    // own turn after that reply. The phone learns a queued message started only
    // from that page, so a message left at the time it was typed (behind the
    // reply's later output) stays hidden as Queued after the agent answers it.
    // The reply is Cupola's own, or one running on the Mac (ChatGPT or Codex
    // there) that Cupola has no message for.
    #[tokio::test]
    async fn queued_message_shows_in_newest_history_once_it_starts() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        for (chat, reply_on_mac) in [
            ("d19b0f7e-97bb-45d7-8206-324a62386dd1", false),
            ("d19b0f7e-97bb-45d7-8206-324a62386dd2", true),
        ] {
            // Client ids are unique per device.
            let client = format!("follow-up-{reply_on_mac}");
            crate::test_support::project_thread(
                &state.store,
                chat,
                "Queue",
                state.data_dir.to_str().unwrap(),
                "claude-fixture",
                Some("claude:sonnet"),
                None,
                "now",
            )
            .await
            .unwrap();
            let item = |turn: &str, id: String| {
                let detail = thread_item_upsert_detail(
                    turn,
                    &serde_json::json!({"id": id, "type": "agentMessage", "text": "working", "phase": "commentary"}),
                    "completed",
                    None,
                );
                (
                    WonderEvent::Activity {
                        category: "thread_item_upsert".into(),
                        state: "updated".into(),
                        detail,
                    },
                    EventContext {
                        conversation_id: Some(chat.into()),
                        thread_id: Some("thread".into()),
                        turn_id: Some(turn.into()),
                        item_id: Some(id),
                        ..EventContext::default()
                    },
                )
            };
            let first = if reply_on_mac {
                let (event, context) = item("turn-1", "mac-start".into());
                publish_event_with_context(&state, event, context)
                    .await
                    .unwrap();
                None
            } else {
                let MessageInsert::Inserted(first) = state
                    .store
                    .insert_message(
                        "owner",
                        "first",
                        "start",
                        "h1",
                        chat,
                        &crate::projects::now_text(),
                    )
                    .await
                    .unwrap()
                else {
                    panic!("first")
                };
                state
                    .store
                    .update_message_delivery(&first.id, "streaming", Some("thread"), Some("turn-1"))
                    .await
                    .unwrap();
                publish_message_state(
                    &state,
                    &first,
                    DeliveryState::Streaming,
                    Some("thread"),
                    Some("turn-1"),
                )
                .await;
                Some(first)
            };
            let MessageInsert::Inserted(follow_up) = state
                .store
                .insert_dispatch_message(
                    "owner",
                    &client,
                    "and then this",
                    "h2",
                    chat,
                    &[],
                    &crate::projects::now_text(),
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("follow-up")
            };
            if !reply_on_mac {
                let receipt = receipt(&state, follow_up.clone()).await.unwrap();
                assert_eq!(receipt.queued, Some(true), "it waits behind the reply");
            }
            // The reply keeps working: more items than one history page holds.
            for index in 0..120 {
                let (event, context) = item("turn-1", format!("step-{index}"));
                publish_event_with_context(&state, event, context)
                    .await
                    .unwrap();
            }
            if let Some(first) = &first {
                state
                    .store
                    .update_message_delivery(&first.id, "completed", None, None)
                    .await
                    .unwrap();
                publish_message_state(
                    &state,
                    first,
                    DeliveryState::Completed,
                    Some("thread"),
                    Some("turn-1"),
                )
                .await;
            }
            // Dispatch's own steps: claim, durable submission, the turn receipt.
            assert!(state
                .store
                .claim_message_for_dispatch(&follow_up.id)
                .await
                .unwrap());
            state
                .store
                .begin_dispatch_submission(&follow_up.id, "thread")
                .await
                .unwrap();
            state
                .store
                .update_message_delivery(
                    &follow_up.id,
                    "accepted_by_codex",
                    Some("thread"),
                    Some("turn-2"),
                )
                .await
                .unwrap();
            publish_message_state(
                &state,
                &follow_up,
                DeliveryState::AcceptedByCodex,
                Some("thread"),
                Some("turn-2"),
            )
            .await;
            let (event, context) = item("turn-2", "answer".into());
            publish_event_with_context(&state, event, context)
                .await
                .unwrap();

            let response = crate::history::conversation_snapshot(
                State(state.clone()),
                Path(chat.into()),
                axum::extract::Query(Default::default()),
                axum::http::HeaderMap::new(),
            )
            .await;
            let page: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 10_000_000).await.unwrap())
                    .unwrap();
            let message = page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["clientMessageId"] == client.as_str());
            assert_eq!(
                message.map(|m| m["state"].clone()),
                Some("accepted_by_codex".into()),
                "the started follow-up is on the newest page (reply on the Mac: {reply_on_mac})"
            );
            let turns = page["thread"]["turns"].as_array().unwrap();
            let position = |turn: &str| turns.iter().position(|t| t["id"] == turn);
            let started = &turns[position("turn-2").expect("its turn")];
            assert_eq!(
                started["items"][0]["payload"]["clientId"],
                client.as_str(),
                "it opens its own turn"
            );
            let reply = position("turn-1").expect("the reply's newest output");
            assert!(
                reply < position("turn-2").unwrap(),
                "it follows the reply it waited for (reply on the Mac: {reply_on_mac})"
            );
        }
    }

    #[tokio::test]
    async fn durable_guide_dispatches_once_and_stale_guide_preserves_text() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        crate::test_support::bind_project_thread(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let MessageInsert::Inserted(active) = state
            .store
            .insert_message(
                "owner",
                "active",
                "work",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("active")
        };
        state
            .store
            .update_message_delivery(&active.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let MessageInsert::Inserted(guide) = state
            .store
            .insert_guide_message(
                "owner",
                "guide",
                "change direction",
                "hash2",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                "turn",
            )
            .await
            .unwrap()
        else {
            panic!("guide")
        };
        state.store.recover_dispatch_claims().await.unwrap();
        assert_eq!(state.store.pending_guides().await.unwrap().len(), 1);
        assert_eq!(
            crate::dispatch_guide(state.clone(), guide.clone(), "turn".into())
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            crate::dispatch_guide(state.clone(), guide.clone(), "turn".into())
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert_eq!(requests.lines().filter(|s| *s == "turn/steer").count(), 1);
        assert_eq!(requests.lines().filter(|s| *s == "turn/start").count(), 0);
        let MessageInsert::Inserted(stale) = state
            .store
            .insert_guide_message(
                "owner",
                "stale",
                "keep these words",
                "hash3",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                "old-turn",
            )
            .await
            .unwrap()
        else {
            panic!("stale")
        };
        // Owner (Send now, Oct 12): a Guide whose reply already ended waits
        // in the queue and is sent next, instead of failing.
        let queued = crate::dispatch_guide(state.clone(), stale.clone(), "old-turn".into()).await;
        assert_eq!(queued.status(), StatusCode::ACCEPTED);
        let receipt: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(queued.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["deliveryState"], "accepted_by_wonder");
        assert_eq!(receipt["queued"], true, "it waits behind the running reply");
        let saved = state.store.message_by_id(&stale.id).await.unwrap().unwrap();
        assert_eq!(saved.body, "keep these words");
        assert_eq!(saved.state, "accepted_by_wonder");
        assert!(state
            .store
            .pending_guides()
            .await
            .unwrap()
            .iter()
            .all(|(m, _)| m.id != stale.id));
        assert!(state
            .store
            .pending_queue("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .iter()
            .any(|item| item.id == stale.id));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}
