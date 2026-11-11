//! T3 Code's titles (owner, Oct 22: "we should use the titles that T3 Code
//! uses for threads and not the Agent SDK's. That way we have consistency if
//! a thread is from T3 Code."). A Claude session or Codex thread T3 Code has
//! (the same exact-ID rule as `T3Code::kept`) is named by T3 Code's title,
//! read from T3 Code's own store, and follows a rename or a regenerated title
//! there.
//!
//! Cupola keeps the title in its own store (`follow_title`), which every
//! surface reads: the Chats row, the header, search and notifications. A
//! thread takes T3 Code's title each time that title changes; a rename in
//! Cupola stays only in Cupola (renames never reach the provider either) and
//! stands until T3 Code's title changes again.
//!
//! The store is read at most every `FRESH`, and again only once T3 Code wrote
//! to it, in one query for every thread. Nothing is written to Cupola's store
//! while a title stays as it was.
use std::time::SystemTime;

use super::*;

/// How long one read of T3 Code's titles answers before the store is looked
/// at again.
const FRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct Titles(tokio::sync::Mutex<Read>);

#[derive(Default)]
struct Read {
    at: Option<Instant>,
    /// The store's and its write-ahead log's change times and sizes as read.
    stamp: Option<Stamp>,
    /// T3 Code's title by (driver, native ID); `None` when more than one
    /// open T3 thread continues the same session (none is its owner).
    titles: HashMap<(String, String), Option<String>>,
    /// Modes of the same uniquely owned session; no permissions are inferred
    /// from Cupola's settings or an ambiguous T3 owner.
    permissions: HashMap<(String, String), Option<Permissions>>,
    /// Each T3 thread that continues a session by (driver, native ID), as
    /// T3 Code's sidebar files it (`Closing`).
    states: HashMap<(String, String), Vec<ThreadState>>,
    /// The T3 Code title each conversation was last given or found to have,
    /// so an unchanged title is never written again.
    followed: HashMap<String, String>,
}

type Stamp = [Option<(SystemTime, u64)>; 2];

type Row = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

/// How T3 Code files one of its threads, from its own store. Its sidebar
/// (read in T3 Code's client, Oct 23) drops a thread with `archivedAt`,
/// shows it under Snoozed while `snoozedUntil` is still ahead, else under
/// Settled when `settledOverride` is "settled" (Settle and auto-settle set
/// it with `settledAt`; Unsettle sets "active" or clears it).
#[derive(Clone, Debug)]
struct ThreadState {
    thread: String,
    archived_at: Option<String>,
    deleted_at: Option<String>,
    settled_at: Option<String>,
    /// Milliseconds since the epoch.
    snoozed_until: Option<i64>,
    snoozed_at: Option<String>,
    /// The T3 thread this one is a subagent of (`lineage.parentThreadId` with
    /// `relationshipToParent` "subagent": what `delegate_task` makes).
    subagent_of: Option<SubagentOf>,
}

impl ThreadState {
    fn of(
        thread: String,
        archived_at: Option<String>,
        deleted_at: Option<String>,
        settled_override: Option<String>,
        settled_at: Option<String>,
        snoozed_until: Option<String>,
        snoozed_at: Option<String>,
    ) -> Self {
        let settled = settled_override.as_deref() == Some("settled");
        Self {
            thread,
            archived_at,
            deleted_at,
            settled_at: settled.then(|| settled_at.unwrap_or_default()),
            snoozed_until: snoozed_until
                .as_deref()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.timestamp_millis()),
            snoozed_at,
            subagent_of: None,
        }
    }

    /// How T3 Code closed this thread `now` (milliseconds), if it did.
    fn closing(&self, now: i64) -> Option<Closing> {
        let (how, since) = if let Some(at) = &self.deleted_at {
            (Closed::Deleted, at.clone())
        } else if let Some(at) = &self.archived_at {
            (Closed::Archived, at.clone())
        } else if let Some(until) = self.snoozed_until.filter(|until| *until > now) {
            (
                Closed::Snoozed(until),
                self.snoozed_at.clone().unwrap_or_else(|| until.to_string()),
            )
        } else if let Some(at) = &self.settled_at {
            (Closed::Settled, at.clone())
        } else {
            return None;
        };
        Some(Closing {
            thread: self.thread.clone(),
            how,
            since,
        })
    }
}

/// How T3 Code closed a thread: out of its sidebar's open threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Closed {
    Settled,
    /// Until this time, in milliseconds since the epoch.
    Snoozed(i64),
    Archived,
    Deleted,
}

/// The T3 thread a subagent thread belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubagentOf {
    pub(crate) parent: String,
    /// T3 Code still has the parent (it is not deleted); an archived parent
    /// still owns its subagents.
    pub(crate) parent_known: bool,
}

/// The T3 thread that closed a session, how, and since when (T3 Code's
/// RFC 3339 time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Closing {
    pub(crate) thread: String,
    pub(crate) how: Closed,
    pub(crate) since: String,
}

impl T3Code {
    /// Same cached, read-only projection as titles (at most once per FRESH).
    /// The T3 mode remains available while its app is closed.
    pub(crate) async fn permissions(
        &self,
        family: AgentFamily,
        native: &str,
    ) -> Option<Permissions> {
        let mut read = self.titles.0.lock().await;
        self.refresh_titles(&mut read).await;
        let driver = t3_driver(family).to_owned();
        // An OpenCode/Cursor chat started in T3 Code is keyed by its T3 thread
        // (`opencode-t3:<thread>`), not by the provider's own session.
        let key = match super::feed::t3_thread(native) {
            Some(thread) => read
                .states
                .iter()
                .find(|((found, _), states)| {
                    *found == driver && states.iter().any(|s| s.thread == thread)
                })
                .map(|(key, _)| key.clone())?,
            None => (driver, native.to_owned()),
        };
        read.permissions.get(&key).cloned().flatten()
    }

    /// T3 Code's title for the T3 thread that continues this Claude session
    /// or Codex thread, as T3 Code shows it.
    pub(crate) async fn title(&self, family: AgentFamily, native: &str) -> Option<String> {
        let mut read = self.titles.0.lock().await;
        self.refresh_titles(&mut read).await;
        read.titles
            .get(&(t3_driver(family).to_owned(), native.to_owned()))
            .cloned()
            .flatten()
    }

    /// How T3 Code closed the T3 thread that continues this Claude session or
    /// Codex thread: none when T3 Code doesn't have it, or any T3 thread that
    /// continues it is open in T3 Code's sidebar.
    pub(crate) async fn closing(&self, family: AgentFamily, native: &str) -> Option<Closing> {
        let mut read = self.titles.0.lock().await;
        self.refresh_titles(&mut read).await;
        let states = read
            .states
            .get(&(t3_driver(family).to_owned(), native.to_owned()))?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut closings = Vec::with_capacity(states.len());
        for state in states {
            closings.push(state.closing(now)?);
        }
        // Settled or snoozed first: Restore can reopen those in T3 Code.
        closings.sort_by_key(|closing| match closing.how {
            Closed::Settled | Closed::Snoozed(_) => 0,
            Closed::Archived => 1,
            Closed::Deleted => 2,
        });
        closings.into_iter().next()
    }

    /// The T3 thread this Claude session or Codex thread is a subagent of
    /// (a `delegate_task` child), read from T3 Code's own lineage for it. A
    /// session that is also an ordinary open T3 thread, or that T3 Code
    /// doesn't have, is none.
    pub(crate) async fn subagent_of(
        &self,
        family: AgentFamily,
        native: &str,
    ) -> Option<SubagentOf> {
        let mut read = self.titles.0.lock().await;
        self.refresh_titles(&mut read).await;
        let states = read
            .states
            .get(&(t3_driver(family).to_owned(), native.to_owned()))?;
        let open = |state: &&ThreadState| state.deleted_at.is_none();
        let mut states = states.iter().filter(open);
        let first = states.next()?;
        // Any plain T3 thread over the same session makes it a chat.
        states
            .all(|state| state.subagent_of.is_some())
            .then(|| first.subagent_of.clone())
            .flatten()
    }

    /// Opens a closed T3 thread again in T3 Code with its own tool
    /// (`t3_thread_organize`: unsettle, unsnooze or unarchive), so T3 Code
    /// shows it too. A deleted thread can't be reopened. The store is read
    /// again at the next look either way.
    pub(crate) async fn reopen(&self, closing: &Closing) -> Result<(), CallError> {
        let action = match closing.how {
            Closed::Settled => "unsettle",
            Closed::Snoozed(_) => "unsnooze",
            Closed::Archived => "unarchive",
            Closed::Deleted => return Err(CallError::NotConnected),
        };
        let outcome = self
            .call(
                "t3_thread_organize",
                json!({"action": action, "threadId": closing.thread}),
            )
            .await
            .map(|_| ());
        self.titles.0.lock().await.at = None;
        outcome
    }

    /// Reads T3 Code's store again at the next look: after a change made
    /// through its tools, and in tests that change it within `FRESH`.
    pub(crate) async fn reread_store(&self) {
        self.titles.0.lock().await.at = None;
    }

    async fn refresh_titles(&self, read: &mut Read) {
        if read.at.is_some_and(|at| at.elapsed() < FRESH) {
            return;
        }
        read.at = Some(Instant::now());
        let Some(home) = &self.home else {
            read.permissions.clear();
            read.titles.clear();
            read.states.clear();
            return;
        };
        let database = home.join(T3_STORE);
        let mut log = database.clone().into_os_string();
        log.push("-wal");
        let seen = |path: PathBuf| async move {
            tokio::fs::metadata(path).await.ok().map(|meta| {
                (
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                )
            })
        };
        let stamp = [seen(database).await, seen(PathBuf::from(log)).await];
        if read.stamp == Some(stamp) {
            return;
        }
        if stamp[0].is_none() {
            read.permissions.clear();
            read.titles.clear();
            read.states.clear();
            read.stamp = Some(stamp);
            return;
        }
        let Some(mut connection) = self.open_store().await else {
            read.permissions.clear();
            return;
        };
        let rows: Result<Vec<Row>, _> = sqlx::query_as(
            "SELECT p.driver, json_extract(p.payload_json, '$.nativeThreadRef.nativeId'), \
             t.thread_id, t.title, t.runtime_mode, t.interaction_mode, t.archived_at, t.deleted_at, \
             json_extract(t.payload_json, '$.settledOverride'), \
             json_extract(t.payload_json, '$.settledAt'), \
             json_extract(t.payload_json, '$.snoozedUntil'), \
             json_extract(t.payload_json, '$.snoozedAt'), \
             CASE WHEN json_extract(t.payload_json, '$.lineage.relationshipToParent') = 'subagent' \
             THEN json_extract(t.payload_json, '$.lineage.parentThreadId') END, \
             EXISTS (SELECT 1 FROM orchestration_v2_projection_threads parent \
             WHERE parent.thread_id = json_extract(t.payload_json, '$.lineage.parentThreadId') \
             AND parent.deleted_at IS NULL) \
             FROM orchestration_v2_projection_provider_threads p \
             JOIN orchestration_v2_projection_threads t ON t.thread_id = p.thread_id \
             WHERE t.active_provider_thread_id IS p.provider_thread_id \
             AND p.driver IS NOT NULL \
             AND json_extract(p.payload_json, '$.nativeThreadRef.nativeId') IS NOT NULL",
        )
        .fetch_all(&mut connection)
        .await;
        let _ = connection.close().await;
        // A busy store is read again next time.
        let Ok(rows) = rows else {
            read.permissions.clear();
            return;
        };
        let mut permissions = HashMap::with_capacity(rows.len());
        let mut titles = HashMap::with_capacity(rows.len());
        let mut states: HashMap<_, Vec<ThreadState>> = HashMap::with_capacity(rows.len());
        for (
            driver,
            native,
            thread,
            title,
            runtime_mode,
            interaction_mode,
            archived,
            deleted,
            settled,
            settled_at,
            until,
            snoozed,
            parent,
            parent_known,
        ) in rows
        {
            let key = (driver, native);
            let mut state = ThreadState::of(
                thread, archived, deleted, settled, settled_at, until, snoozed,
            );
            state.subagent_of = parent.map(|parent| SubagentOf {
                parent,
                parent_known,
            });
            if state.archived_at.is_none() && state.deleted_at.is_none() {
                permissions
                    .entry(key.clone())
                    .and_modify(|found| *found = None)
                    .or_insert(Some(Permissions {
                        runtime_mode,
                        interaction_mode,
                    }));
                titles
                    .entry(key.clone())
                    .and_modify(|found| *found = None)
                    .or_insert(Some(title));
            }
            states.entry(key).or_default().push(state);
        }
        read.permissions = permissions;
        read.titles = titles;
        read.states = states;
        read.stamp = Some(stamp);
    }

    /// T3 Code's own store, only read: while T3 Code runs, its write-ahead
    /// log exists and is read with it; otherwise the file is read as
    /// unchanging, which creates no files beside it.
    pub(super) async fn open_store(&self) -> Option<sqlx::SqliteConnection> {
        let database = self.home.as_ref()?.join(T3_STORE);
        let mut log = database.clone().into_os_string();
        log.push("-wal");
        if !tokio::fs::metadata(&database)
            .await
            .is_ok_and(|meta| meta.is_file())
        {
            return None;
        }
        let running = tokio::fs::try_exists(&log).await.unwrap_or(false);
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database)
            .read_only(true)
            .immutable(!running)
            .busy_timeout(Duration::from_millis(25));
        sqlx::SqliteConnection::connect_with(&options).await.ok()
    }
}

/// A title T3 Code gave a thread, as Cupola shows titles; none when T3 Code
/// only copied the first thing the agent was sent. T3 Code names a session
/// it found on the Mac by its first message, which for most Codex threads is
/// the instructions Codex sends ahead of it ("# AGENTS.md instructions for
/// …") or context in tags (`<pasted_content …>`); Cupola's own title for
/// those (the provider's name, else the owner's first message) says more.
fn shown(title: &str) -> Option<String> {
    let title = title.trim_start();
    if title.starts_with("# AGENTS.md instructions") || title.starts_with('<') {
        return None;
    }
    let shown = crate::projects::display_title(None, Some(title));
    (shown != "Untitled thread").then_some(shown)
}

/// The title of a conversation whose session T3 Code has, once T3 Code's
/// title changed since Cupola last took it: it is stored as the thread's
/// title (Cupola's single title, which every surface reads) and returned.
/// `None` when T3 Code has no title for it, or the stored title is current
/// (it may be a rename in Cupola, which stands until T3 Code's changes).
pub(crate) async fn follow_title(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Option<String> {
    let native = conversation.native_session_id.as_deref()?;
    let t3 = &state.t3_code;
    let title = shown(&t3.title(conversation.family, native).await?)?;
    let id = &conversation.conversation_id;
    if t3.titles.0.lock().await.followed.get(id) == Some(&title) {
        return None;
    }
    let taken = state.store.follow_t3_code_title(id, &title).await.ok()?;
    t3.titles
        .0
        .lock()
        .await
        .followed
        .insert(id.clone(), title.clone());
    taken.then_some(title)
}

/// The title of a session no conversation owns yet: T3 Code's, when T3 Code
/// has it, else the provider's.
pub(crate) async fn listed_title(
    state: &AppState,
    family: AgentFamily,
    native: &str,
    provider: String,
) -> String {
    match state.t3_code.title(family, native).await {
        Some(title) => shown(&title).unwrap_or(provider),
        None => provider,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::Fake;
    use super::*;

    async fn titled(fake: &Fake, thread: &str, title: &str) {
        let mut store = fake.store().await;
        sqlx::query("UPDATE orchestration_v2_projection_threads SET title = ? WHERE thread_id = ?")
            .bind(title)
            .bind(thread)
            .execute(&mut store)
            .await
            .unwrap();
        store.close().await.unwrap();
    }

    fn fresh(t3: &T3Code) {
        t3.titles.0.try_lock().unwrap().at = None;
    }

    async fn chat(state: &AppState, id: &str, model: Option<&str>, native: &str, title: &str) {
        crate::test_support::project_thread(
            &state.store,
            id,
            title,
            state.data_dir.to_str().unwrap(),
            "home",
            model,
            None,
            "2026-10-22T10:00:00Z",
        )
        .await
        .unwrap();
        state
            .store
            .set_project_native_session(id, native, "2026-10-22T10:00:00Z")
            .await
            .unwrap();
    }

    async fn shown_title(state: &AppState, id: &str) -> (String, StoredProjectConversation) {
        let conversation = state.store.project_conversation(id).await.unwrap().unwrap();
        let title = crate::projects::settled_title(state, &conversation, None).await;
        let stored = state.store.project_conversation(id).await.unwrap().unwrap();
        (title, stored)
    }

    // Contract (owner, Oct 22): a chat whose Claude session or Codex thread
    // T3 Code has is titled as T3 Code titles it, in Cupola's stored title
    // that every surface reads (rows, header, search, notifications), and
    // follows T3 Code's renames; any other chat, or every chat without T3
    // Code's store, keeps its own title. An unchanged title is never written
    // again, and a rename in Cupola stands until T3 Code's title changes.
    #[tokio::test]
    async fn a_chat_t3_code_has_is_titled_as_t3_code_titles_it() {
        let (_dir, mut state) = crate::ingestion::tests::fixture().await;
        let fake = Fake::default();
        let mut t3 = T3Code::disabled();
        t3.home = Some(fake.home.0.path().to_owned());
        state.t3_code = Arc::new(t3);
        chat(
            &state,
            "claude-chat",
            Some("claude:haiku"),
            "claude-1",
            "Agent SDK summary",
        )
        .await;
        chat(&state, "codex-chat", None, "codex-1", "Codex thread name").await;
        chat(
            &state,
            "own-chat",
            Some("claude:haiku"),
            "claude-9",
            "Cupola only",
        )
        .await;

        assert_eq!(
            shown_title(&state, "claude-chat").await.0,
            "Agent SDK summary",
            "no store"
        );

        fake.keeps("t3-a", "claudeAgent", "claude-1").await;
        fake.keeps("t3-b", "codex", "codex-1").await;
        titled(
            &fake,
            "t3-a",
            "Audit Provider Updates and Agent Reliability",
        )
        .await;
        titled(&fake, "t3-b", "Fix the queue").await;
        fresh(&state.t3_code);
        let (title, stored) = shown_title(&state, "claude-chat").await;
        assert_eq!(title, "Audit Provider Updates and Agent Reliability");
        assert_eq!(stored.title, title, "stored for every surface");
        let (title, stored) = shown_title(&state, "codex-chat").await;
        assert_eq!(
            (title.as_str(), stored.title.as_str()),
            ("Fix the queue", "Fix the queue")
        );
        assert_eq!(shown_title(&state, "own-chat").await.0, "Cupola only");
        assert_eq!(
            listed_title(&state, AgentFamily::Codex, "codex-1", "Codex name".into()).await,
            "Fix the queue",
            "a session no chat owns yet"
        );
        assert_eq!(
            listed_title(&state, AgentFamily::Codex, "codex-2", "Codex name".into()).await,
            "Codex name"
        );

        // Unchanged: nothing is written.
        let before = state
            .store
            .project_conversation("claude-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(follow_title(&state, &before).await, None);
        fresh(&state.t3_code);
        assert_eq!(follow_title(&state, &before).await, None);
        assert_eq!(shown_title(&state, "claude-chat").await.1, before);

        // A rename in Cupola stands until T3 Code's title changes.
        state
            .store
            .update_project_conversation(
                "claude-chat",
                wonder_store::ProjectConversationPatch {
                    title: Some("My name"),
                    ..Default::default()
                },
                "2026-10-22T11:00:00Z",
            )
            .await
            .unwrap();
        fresh(&state.t3_code);
        assert_eq!(shown_title(&state, "claude-chat").await.0, "My name");
        titled(&fake, "t3-a", "Audit Agent Reliability").await;
        fresh(&state.t3_code);
        let (title, stored) = shown_title(&state, "claude-chat").await;
        assert_eq!(title, "Audit Agent Reliability", "T3 Code renamed it");
        assert_eq!(stored.title, title);
    }

    // Contract (owner, Oct 22): a Claude session or Codex thread T3 Code has
    // is named by T3 Code's title, which follows a rename there; a session it
    // doesn't have, or no store at all, has no T3 Code title. T3 Code's copy
    // of the instructions Codex sent first is not a title.
    #[tokio::test]
    async fn t3_codes_title_names_the_sessions_it_has() {
        let fake = Fake::default();
        let mut t3 = T3Code::disabled();
        t3.home = Some(fake.home.0.path().to_owned());
        assert_eq!(
            t3.title(AgentFamily::Claude, "claude-1").await,
            None,
            "no store"
        );
        fake.keeps("t3-a", "claudeAgent", "claude-1").await;
        fake.keeps("t3-b", "codex", "codex-1").await;
        titled(&fake, "t3-a", "Audit Provider Updates").await;
        titled(&fake, "t3-b", "Fix the queue").await;
        fresh(&t3);
        assert_eq!(
            t3.title(AgentFamily::Claude, "claude-1").await.as_deref(),
            Some("Audit Provider Updates")
        );
        assert_eq!(
            t3.title(AgentFamily::Codex, "codex-1").await.as_deref(),
            Some("Fix the queue")
        );
        assert_eq!(t3.title(AgentFamily::Claude, "codex-1").await, None);
        assert_eq!(t3.title(AgentFamily::Claude, "claude-2").await, None);

        // T3 Code runs and regenerates a title: read once the store changed.
        let running = fake.store().await;
        titled(&fake, "t3-a", "Audit Agent Reliability").await;
        assert_eq!(
            t3.title(AgentFamily::Claude, "claude-1").await.as_deref(),
            Some("Audit Provider Updates"),
            "read again only after a while"
        );
        fresh(&t3);
        assert_eq!(
            t3.title(AgentFamily::Claude, "claude-1").await.as_deref(),
            Some("Audit Agent Reliability")
        );
        running.close().await.unwrap();

        assert_eq!(shown("# AGENTS.md instructions for /Users/me/Work"), None);
        assert_eq!(shown("<pasted_content id=\"c5b7\">"), None);
        assert_eq!(shown("Fix the queue").as_deref(), Some("Fix the queue"));
    }
}
