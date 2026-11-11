use super::*;
use tower::ServiceExt;
/// Searches until no source is partial. A search waits only 1.5 s for the
/// transcript index, which a busy Mac can outlast; the next search resumes it.
async fn complete_search(state: &AppState, query: &str) -> crate::native_search::SearchOutcome {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let outcome =
                crate::native_search::search(state, query, None, Default::default(), 20, true)
                    .await
                    .unwrap();
            if !outcome.partial {
                return outcome;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the transcript index never completed")
}
async fn get(state: &AppState, uri: &str, owner: bool) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if owner {
        request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
    }
    let response = router(state.clone())
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}
#[tokio::test]
async fn search_pages_require_owner_and_bind_cursor_to_query_scope_and_epoch() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::project_thread(
        &state.store,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        "Project",
        state.data_dir.to_str().unwrap(),
        crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
        None,
        None,
        "1",
    )
    .await
    .unwrap();
    for n in 0..7 {
        state
            .store
            .insert_message(
                "owner",
                &format!("search-{n}"),
                "needle message",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "1000",
            )
            .await
            .unwrap();
    }
    assert_ne!(
        get(&state, "/api/v1/search?q=needle", false).await.0,
        StatusCode::OK
    );
    let (status, first) = get(
        &state,
        "/api/v1/search?q=needle&conversationId=d19b0f7e-97bb-45d7-8206-324a62386dd9&limit=3",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/schemas/wonder-http-v1.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serde_json::json!({"apiVersion":"v1","searchResponse":first})));
    assert_eq!(first["results"].as_array().unwrap().len(), 3);
    let cursor = first["nextCursor"].as_str().unwrap();
    let (status, next) = get(
        &state,
        &format!("/api/v1/search?q=needle&conversationId=d19b0f7e-97bb-45d7-8206-324a62386dd9&limit=3&cursor={cursor}"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for item in first["results"].as_array().unwrap() {
        assert!(!next["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == item["id"]));
        assert_eq!(item["kind"], "message");
        assert_eq!(
            item["conversationId"],
            "d19b0f7e-97bb-45d7-8206-324a62386dd9"
        );
    }
    for uri in [
        format!("/api/v1/search?q=other&conversationId=d19b0f7e-97bb-45d7-8206-324a62386dd9&cursor={cursor}"),
        format!("/api/v1/search?q=needle&cursor={cursor}"),
        "/api/v1/search?q=needle&cursor=garbage".into(),
        "/api/v1/search?q=needle&deviceId=forged".into(),
        "/api/v1/search?q=needle&limit=0".into(),
    ] {
        assert_eq!(
            get(&state, &uri, true).await.0,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    let mut restarted = state.clone();
    restarted.host_epoch = "changed".into();
    assert_eq!(
        get(
            &restarted,
            &format!("/api/v1/search?q=needle&conversationId=d19b0f7e-97bb-45d7-8206-324a62386dd9&cursor={cursor}"),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

async fn bind(
    state: &AppState,
    id: &str,
    project: &str,
    family: wonder_store::AgentFamily,
    native: &str,
    cwd: &str,
) {
    let provider_store = crate::providers::provider_store(state, family).to_owned();
    state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: id,
            project_id: project,
            family,
            provider_store: &provider_store,
            native_session_id: Some(native),
            cwd,
            roots_revision: 1,
            title: id,
            model: None,
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now: "1",
        })
        .await
        .unwrap();
}

// Contract: unscoped search also finds the Mac's own Codex threads and Claude
// Code sessions in included Projects, newest first with Cupola's results, and
// a session bound to a Cupola chat appears once: as that chat, and not at all
// when Cupola's own results already cover it.
#[tokio::test]
async fn native_history_merges_once_and_opens_where_it_lives() {
    use wonder_store::AgentFamily::{Claude, Codex};
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let root = crate::canonical_path::canonicalize(dir.path().join(chat)).unwrap();
    let root = root.to_str().unwrap();
    let project = format!("project-{chat}");
    bind(&state, "covered", &project, Codex, "c0000000-0001", root).await;
    bind(&state, "quiet", &project, Codex, "c0000000-0002", root).await;
    bind(&state, "archived", &project, Codex, "c0000000-0003", root).await;
    bind(
        &state,
        "claude-covered",
        &project,
        Claude,
        "b0000000-0002",
        root,
    )
    .await;
    state
        .store
        .set_project_conversation_wonder_archived("archived", true, "2")
        .await
        .unwrap();
    for (n, conversation) in [chat, chat, "covered", "claude-covered"].iter().enumerate() {
        state
            .store
            .insert_message(
                "owner",
                &format!("m{n}"),
                "needle message",
                "hash",
                conversation,
                "1000",
            )
            .await
            .unwrap();
    }
    let thread = |id: &str, at: i64, cwd: &str, parent: Option<&str>| {
        serde_json::json!({"snippet": format!("found the Needle in {id}"),
            "thread": {"id": id, "cwd": cwd, "updatedAt": at, "parentThreadId": parent, "name": format!("Thread {id}")}})
    };
    std::fs::write(
        dir.path().join("thread-search.json"),
        serde_json::json!({"pageSize": 2, "results": [
            thread("c0000000-0001", 1_700_000_006, root, None),
            thread("c0000000-0005", 1_700_000_005, root, Some("c0000000-0004")),
            thread("c0000000-0002", 1_700_000_004, root, None),
            thread("c0000000-0006", 1_700_000_003, "/elsewhere", None),
            thread("c0000000-0003", 1_700_000_002, root, None),
            thread("c0000000-0004", 1_700_000_001, root, None),
        ], "occurrences": {"c0000000-0002": [
            {"itemId": "item-7", "turnId": "turn-3", "turnCursor": "x", "snippet": "a needle here",
             "snippetMatchRange": {"start": 2, "end": 8}}
        ]}})
        .to_string(),
    )
    .unwrap();
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/schemas/wonder-http-v1.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    // The app decodes this shared example (SearchTests.swift).
    let example: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/search-response-v1.json"
    ))
    .unwrap();
    assert!(validator.is_valid(&serde_json::json!({"apiVersion":"v1","searchResponse":example})));
    let mut cursor: Option<String> = None;
    let mut seen = Vec::new();
    for _ in 0..10 {
        let uri = match &cursor {
            Some(cursor) => format!("/api/v1/search?q=needle&limit=2&cursor={cursor}"),
            None => "/api/v1/search?q=needle&limit=2".into(),
        };
        let (status, page) = get(&state, &uri, true).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            validator.is_valid(&serde_json::json!({"apiVersion":"v1","searchResponse":page})),
            "{page}"
        );
        assert_eq!(page["partial"], false);
        seen.extend(page["results"].as_array().unwrap().iter().cloned());
        match page["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    let keys: Vec<(String, String)> = seen
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().into(),
                r["id"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        keys.iter()
            .map(|(kind, id)| format!("{kind}:{id}"))
            .take(2)
            .collect::<Vec<_>>(),
        ["thread:c0000000-0002", "thread:c0000000-0004"],
        "native threads are newer than the fixture messages"
    );
    assert_eq!(keys.len(), 6, "{keys:?}");
    assert_eq!(keys.iter().filter(|(kind, _)| kind == "message").count(), 4);
    let quiet = &seen[0];
    assert_eq!(quiet["conversationId"], "quiet");
    assert_eq!(quiet["deepLink"], "#/chats/quiet");
    assert_eq!(
        quiet["snippet"],
        "found the <mark>Needle</mark> in c0000000-0002"
    );
    let unbound = &seen[1];
    assert_eq!(unbound["conversationId"], serde_json::Value::Null);
    assert_eq!(unbound["projectId"], project);
    assert_eq!(unbound["reference"], "codex:c0000000-0004");
    assert_eq!(unbound["family"], "codex");

    // A cursor from before native results (version 1) restarts the search.
    let old = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&(
            1,
            "epoch",
            "needle",
            None::<String>,
            serde_json::json!({"sort_ms":1,"kind":"message","id":"x"}),
        ))
        .unwrap(),
    );
    assert_eq!(
        get(
            &state,
            &format!("/api/v1/search?q=needle&cursor={old}"),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // In-thread occurrences open a native result at its message.
    let (status, found) = get(
        &state,
        "/api/v1/conversations/quiet/search/occurrences?q=needle",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert!(validator
        .is_valid(&serde_json::json!({"apiVersion":"v1","searchOccurrencesResponse":found})));
    assert_eq!(found["occurrences"][0]["itemId"], "item-7");
    assert_eq!(
        found["occurrences"][0]["snippet"],
        "a <mark>needle</mark> here"
    );
    let (_, wonder_only) = get(
        &state,
        &format!("/api/v1/conversations/{chat}/search/occurrences?q=needle"),
        true,
    )
    .await;
    assert_eq!(
        wonder_only,
        serde_json::json!({"supported": true, "occurrences": []})
    );
    assert_ne!(
        get(
            &state,
            "/api/v1/conversations/quiet/search/occurrences?q=needle",
            false
        )
        .await
        .0,
        StatusCode::OK
    );

    // Claude Code transcripts in the Project's folder are searched from the
    // local cache; the session a covered chat is bound to is not repeated.
    let folder = dir.path().join("projects/-fixture");
    std::fs::create_dir_all(&folder).unwrap();
    for session in ["b0000000-0001", "b0000000-0002"] {
        std::fs::write(
            folder.join(format!("{session}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({"type":"user","uuid":format!("u-{session}"),"cwd":root,
                "timestamp":"2023-11-14T22:13:20.500Z","message":{"content":"a needle prompt"}})
            ),
        )
        .unwrap();
    }
    // A renamed session whose name is in none of its messages.
    std::fs::write(
        folder.join("b0000000-0003.jsonl"),
        [
            serde_json::json!({"type":"user","uuid":"u-b0000000-0003","cwd":root,
                "timestamp":"2023-11-14T22:13:21.500Z","message":{"content":"fix the build"}}),
            serde_json::json!({"type":"custom-title","customTitle":"Quarterly walrus plan",
                "sessionId":"b0000000-0003"}),
        ]
        .map(|line| format!("{line}\n"))
        .concat(),
    )
    .unwrap();
    let outcome = complete_search(&state, "needle").await;
    let claude: Vec<_> = outcome
        .hits
        .iter()
        .filter(|hit| hit.kind == "native_message")
        .collect();
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0].reference.as_deref(), Some("claude:b0000000-0001"));
    assert_eq!(claude[0].item_id.as_deref(), Some("u-b0000000-0001"));
    assert_eq!(
        claude[0].snippet.as_deref(),
        Some("a <mark>needle</mark> prompt")
    );
    assert_eq!(outcome.hits.len(), 7);
    let named = complete_search(&state, "walrus plan").await;
    // (The fake Codex search answers every query.)
    let named: Vec<_> = named
        .hits
        .iter()
        .filter(|hit| hit.family == Some(Claude))
        .collect();
    assert_eq!(named.len(), 1, "{named:?}");
    let hit = named[0];
    assert_eq!(hit.kind, "thread");
    assert_eq!(hit.title, "Quarterly walrus plan");
    assert_eq!(hit.reference.as_deref(), Some("claude:b0000000-0003"));
    assert_eq!(hit.project_id.as_deref(), Some(project.as_str()));
    assert_eq!(hit.snippet, None);

    // A runtime without thread search leaves Cupola's own results, complete.
    std::fs::remove_file(dir.path().join("thread-search.json")).unwrap();
    let (status, page) = get(&state, "/api/v1/search?q=needle&limit=50", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["partial"], false);
    assert_eq!(page["results"].as_array().unwrap().len(), 4);
    let (_, unsupported) = get(
        &state,
        "/api/v1/conversations/quiet/search/occurrences?q=needle",
        true,
    )
    .await;
    assert_eq!(unsupported["supported"], false);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
