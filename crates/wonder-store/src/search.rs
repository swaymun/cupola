//! Persisted search with deterministic keyset pagination; no runtime history fetches.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchCursor {
    pub sort_ms: i64,
    pub kind: String,
    pub id: String,
}
pub struct SearchPage {
    pub results: Vec<StoredSearchResult>,
    /// The keyset position after each result, so a caller merging this page
    /// with other sources can resume exactly after the last one it used.
    pub positions: Vec<SearchCursor>,
    pub next_cursor: Option<SearchCursor>,
    /// Conversations with any match (unscoped searches only), so results
    /// from a thread's native history can defer to Wonder's own.
    pub matched_conversations: std::collections::BTreeSet<String>,
}
impl Store {
    pub async fn search(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<StoredSearchResult>, sqlx::Error> {
        Ok(self.search_page(query, None, None, limit).await?.results)
    }

    pub async fn search_page(
        &self,
        query: &str,
        conversation: Option<&str>,
        before: Option<&SearchCursor>,
        limit: u32,
    ) -> Result<SearchPage, sqlx::Error> {
        let match_query = search_match_query(query);
        if match_query.is_empty() || limit == 0 {
            return Ok(SearchPage {
                results: vec![],
                positions: vec![],
                next_cursor: None,
                matched_conversations: Default::default(),
            });
        }
        self.reindex_stale_search_conversations().await?;
        // One read snapshot: the page and the matched chats agree even while
        // another search reindexes.
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(r#"
            WITH matched AS (
                SELECT kind,result_id,search_documents.title AS title,
                    snippet(search_documents,3,'<mark>','</mark>','…',12) AS snippet,
                    search_documents.conversation_id AS conversation_id,search_documents.updated_at AS updated_at,
                    CASE WHEN search_documents.updated_at NOT GLOB '*[^0-9]*' THEN CAST(search_documents.updated_at AS INTEGER)
                    ELSE COALESCE(CAST((julianday(search_documents.updated_at)-2440587.5)*86400000 AS INTEGER),0) END AS sort_ms
                FROM search_documents
                JOIN project_conversations c ON c.conversation_id=search_documents.conversation_id
                JOIN projects p ON p.id=c.project_id
                WHERE search_documents MATCH ? AND p.is_included=1 AND c.wonder_archived_at IS NULL AND NOT EXISTS(SELECT 1 FROM rewind_operations r WHERE r.conversation_id=c.conversation_id)
                    AND (? IS NULL OR search_documents.conversation_id=?)
            )
            SELECT * FROM matched WHERE ? IS NULL OR (sort_ms,kind,result_id)<(?,?,?)
            ORDER BY sort_ms DESC,kind DESC,result_id DESC LIMIT ?
        "#)
        .bind(&match_query).bind(conversation).bind(conversation)
        .bind(before.map(|c| c.sort_ms)).bind(before.map(|c| c.sort_ms))
        .bind(before.map(|c| &c.kind)).bind(before.map(|c| &c.id))
        .bind(i64::from(limit.min(100)) + 1)
        .fetch_all(&mut *transaction).await?;
        let matched_conversations = if conversation.is_none() {
            sqlx::query_scalar::<_, String>("SELECT DISTINCT search_documents.conversation_id FROM search_documents JOIN project_conversations c ON c.conversation_id=search_documents.conversation_id JOIN projects p ON p.id=c.project_id WHERE search_documents MATCH ? AND p.is_included=1 AND c.wonder_archived_at IS NULL AND NOT EXISTS(SELECT 1 FROM rewind_operations r WHERE r.conversation_id=c.conversation_id)")
                .bind(&match_query)
                .fetch_all(&mut *transaction)
                .await?
                .into_iter()
                .collect()
        } else {
            Default::default()
        };
        transaction.commit().await?;
        let has_more = rows.len() > limit.min(100) as usize;
        let mut positions = Vec::new();
        let results = rows
            .into_iter()
            .take(limit.min(100) as usize)
            .map(|row| {
                positions.push(SearchCursor {
                    sort_ms: row.get("sort_ms"),
                    kind: row.get("kind"),
                    id: row.get("result_id"),
                });
                StoredSearchResult {
                    kind: row.get("kind"),
                    id: row.get("result_id"),
                    title: row.get("title"),
                    snippet: row.get("snippet"),
                    conversation_id: row.get("conversation_id"),
                    updated_at: row.get("updated_at"),
                }
            })
            .collect();
        Ok(SearchPage {
            results,
            next_cursor: if has_more {
                positions.last().cloned()
            } else {
                None
            },
            positions,
            matched_conversations,
        })
    }

    /// Writers mark the chat whose searchable records changed; this replaces
    /// only those chats' documents. With nothing marked, searching takes no
    /// writer lock.
    async fn reindex_stale_search_conversations(&self) -> Result<(), sqlx::Error> {
        let stale: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM search_stale_conversations)")
                .fetch_one(&self.pool)
                .await?;
        if !stale {
            return Ok(());
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM search_documents WHERE rowid IN (SELECT o.doc_rowid FROM search_document_owners o JOIN search_stale_conversations s ON s.conversation_id=o.conversation_id)")
            .execute(&mut *transaction).await?;
        sqlx::query("DELETE FROM search_document_owners WHERE conversation_id IN (SELECT conversation_id FROM search_stale_conversations)")
            .execute(&mut *transaction).await?;
        // Fresh documents get rowids above every remaining one, so their
        // owners are exactly the rows past this mark.
        let mark: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(doc_rowid),0) FROM search_document_owners")
                .fetch_one(&mut *transaction)
                .await?;
        sqlx::query(r#"
            INSERT INTO search_documents(rowid,kind,result_id,title,body,conversation_id,updated_at)
            SELECT ?+row_number() OVER (),kind,result_id,title,body,conversation_id,updated_at FROM (
                SELECT 'conversation' AS kind,c.conversation_id AS result_id,c.title AS title,c.title AS body,c.conversation_id AS conversation_id,c.updated_at AS updated_at
                    FROM project_conversations c JOIN search_stale_conversations s ON s.conversation_id=c.conversation_id
                UNION ALL SELECT 'message',m.id,c.title,m.body,m.conversation_id,m.created_at
                    FROM messages m JOIN project_conversations c ON c.conversation_id=m.conversation_id JOIN search_stale_conversations s ON s.conversation_id=c.conversation_id
                UNION ALL SELECT 'assistant_message',m.id,c.title,m.text,m.conversation_id,m.updated_at
                    FROM assistant_messages m JOIN project_conversations c ON c.conversation_id=m.conversation_id JOIN search_stale_conversations s ON s.conversation_id=c.conversation_id
                UNION ALL SELECT 'file',f.id,f.name,f.name || ' ' || COALESCE(f.relative_path,''),f.conversation_id,f.updated_at
                    FROM conversation_files f JOIN project_conversations c ON c.conversation_id=f.conversation_id JOIN search_stale_conversations s ON s.conversation_id=c.conversation_id
            )
        "#)
        .bind(mark)
        .execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO search_document_owners(doc_rowid,conversation_id) SELECT rowid,conversation_id FROM search_documents WHERE rowid>?")
            .bind(mark)
            .execute(&mut *transaction).await?;
        sqlx::query("DELETE FROM search_stale_conversations")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn tied_pages_filter_scope_and_hidden_content_and_refresh_edits() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "1")
            .await
            .unwrap();
        seed_project_thread(
            &store,
            "project",
            "Project",
            "/tmp/project",
            None,
            None,
            "1",
        )
        .await
        .unwrap();
        seed_project_thread(&store, "chat", "Chat", "/tmp/chat", None, None, "1")
            .await
            .unwrap();
        seed_project_thread(&store, "other", "Other", "/tmp/other", None, None, "1")
            .await
            .unwrap();
        seed_project_thread(
            &store,
            "archived",
            "Archived",
            "/tmp/archived",
            None,
            None,
            "1",
        )
        .await
        .unwrap();
        store
            .set_project_conversation_wonder_archived("archived", true, "2")
            .await
            .unwrap();
        store
            .bind_project_runtime(
                "chat",
                AgentFamily::Codex,
                "fixture-store",
                "parent-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        store
            .register_subagent_ownership(
                "hidden",
                "chat",
                "child-thread",
                "parent-thread",
                None,
                None,
                None,
                "{}",
                None,
                None,
                "idle",
                None,
                "now",
            )
            .await
            .unwrap();
        let mut expected = std::collections::BTreeSet::new();
        let mut first_id = String::new();
        for n in 0..23 {
            let MessageInsert::Inserted(m) = store
                .insert_message(
                    "owner",
                    &format!("message-{n}"),
                    "A needle matching message",
                    "hash",
                    "chat",
                    "1000",
                )
                .await
                .unwrap()
            else {
                panic!()
            };
            if n == 0 {
                first_id = m.id.clone();
            }
            expected.insert(("message".to_owned(), m.id));
        }
        for n in 0..3 {
            let assistant = store
                .complete_assistant_message(
                    "chat",
                    "thread",
                    &format!("turn-{n}"),
                    "answer",
                    "A needle assistant response",
                    "1000",
                )
                .await
                .unwrap()
                .unwrap();
            expected.insert(("assistant_message".to_owned(), assistant.id));
        }
        for (client, conversation) in [
            ("other-message", "other"),
            ("archived-message", "archived"),
            ("hidden-message", "hidden"),
        ] {
            store
                .insert_message(
                    "owner",
                    client,
                    "needle excluded from scoped results",
                    "hash",
                    conversation,
                    "1000",
                )
                .await
                .unwrap();
        }
        let mut cursor = None;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let page = store
                .search_page("needle", Some("chat"), cursor.as_ref(), 4)
                .await
                .unwrap();
            for result in page.results {
                assert_eq!(result.conversation_id.as_deref(), Some("chat"));
                assert!(result.snippet.unwrap().contains("<mark>needle</mark>"));
                assert!(
                    seen.insert((result.kind, result.id)),
                    "no duplicate across tied pages"
                );
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen, expected, "no gaps across tied pages");
        assert!(store
            .search_page("needle", Some("hidden"), None, 10)
            .await
            .unwrap()
            .results
            .is_empty());
        assert!(store
            .search_page("needle", Some("archived"), None, 10)
            .await
            .unwrap()
            .results
            .is_empty());
        assert!(store
            .search_page("needle", Some("missing"), None, 10)
            .await
            .unwrap()
            .results
            .is_empty());
        // Native history defers to these; hidden and archived chats are not among them.
        assert_eq!(
            store
                .search_page("needle", None, None, 1)
                .await
                .unwrap()
                .matched_conversations,
            ["chat".to_owned(), "other".to_owned()].into()
        );
        let all = store.search("needle", 100).await.unwrap();
        assert_eq!(all.len(), 27);
        assert!(
            all.iter()
                .all(|r| matches!(r.kind.as_str(), "message" | "assistant_message")),
            "system prompts are not searchable message content"
        );
        sqlx::query("UPDATE messages SET body='changed away from query' WHERE id=?")
            .bind(&first_id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .search_page("needle", Some("chat"), None, 100)
                .await
                .unwrap()
                .results
                .len(),
            25
        );
        sqlx::query(
            "DELETE FROM messages WHERE device_id='owner' AND client_message_id='message-1'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            store
                .search_page("needle", Some("chat"), None, 100)
                .await
                .unwrap()
                .results
                .len(),
            24
        );
    }

    // Searching reindexes only chats whose records changed, takes no writer
    // lock when nothing changed, and hides archived chats and hidden Projects
    // at query time so restoring them needs no reindex.
    #[tokio::test]
    async fn search_reindexes_changed_chats_and_filters_visibility_when_searching() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("search.db").display()
        ))
        .await
        .unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "1")
            .await
            .unwrap();
        for chat in ["first", "second"] {
            seed_project_thread(&store, chat, chat, "/tmp/search", None, None, "1")
                .await
                .unwrap();
            store
                .insert_message("owner", chat, "a needle here", "hash", chat, "1000")
                .await
                .unwrap();
        }
        let found = |store: Store| async move {
            let mut ids: Vec<String> = store
                .search("needle", 100)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|result| result.conversation_id)
                .collect();
            ids.sort();
            ids
        };
        let owners = |store: Store, chat: &'static str| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT doc_rowid FROM search_document_owners WHERE conversation_id=? ORDER BY doc_rowid",
            )
            .bind(chat)
            .fetch_all(&store.pool)
            .await
            .unwrap()
        };
        assert_eq!(found(store.clone()).await, ["first", "second"]);
        let first_docs = owners(store.clone(), "first").await;
        assert_eq!(first_docs.len(), 2, "the chat and its message");

        // With nothing changed, a search succeeds while another writer holds
        // the lock; a rebuild would wait out the busy timeout and fail.
        let mut writer = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        assert_eq!(found(store.clone()).await, ["first", "second"]);
        sqlx::query("ROLLBACK").execute(&mut *writer).await.unwrap();
        drop(writer);

        // An edit in one chat leaves the other chat's documents in place.
        store
            .insert_message(
                "owner",
                "second-2",
                "another needle",
                "hash",
                "second",
                "1001",
            )
            .await
            .unwrap();
        assert_eq!(store.search("needle", 100).await.unwrap().len(), 3);
        assert_eq!(owners(store.clone(), "first").await, first_docs);
        assert_eq!(owners(store.clone(), "second").await.len(), 3);

        // Archive and Project visibility apply without a reindex either way.
        store
            .set_project_conversation_wonder_archived("first", true, "2")
            .await
            .unwrap();
        assert_eq!(found(store.clone()).await, ["second", "second"]);
        store
            .set_project_conversation_wonder_archived("first", false, "3")
            .await
            .unwrap();
        assert_eq!(found(store.clone()).await, ["first", "second", "second"]);
        sqlx::query("UPDATE projects SET is_included=0 WHERE id='project-second'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(found(store.clone()).await, ["first"]);
        sqlx::query("UPDATE projects SET is_included=1 WHERE id='project-second'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(found(store.clone()).await, ["first", "second", "second"]);

        // A renamed chat is found by its new name.
        sqlx::query(
            "UPDATE project_conversations SET title='Haystack' WHERE conversation_id='first'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let renamed = store.search("haystack", 100).await.unwrap();
        assert!(renamed
            .iter()
            .any(|result| result.kind == "conversation" && result.id == "first"));

        // Reset leaves no documents behind and later searches still work.
        store.reset_workspace("owner", "4").await.unwrap();
        assert!(store.search("needle", 100).await.unwrap().is_empty());
        for table in [
            "search_documents",
            "search_document_owners",
            "search_stale_conversations",
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT COUNT(*) FROM {table}"
                )))
                .fetch_one(&store.pool)
                .await
                .unwrap(),
                0,
                "{table}"
            );
        }
    }
}
