//! Auto-archive, modelled on T3 Code's thread settlement but archiving for
//! real (owner decision 2026-10-20): a Project thread is archived through the
//! same path as the phone's Archive once it has been idle for the rule's days,
//! or once a pull request it opened merged after the thread's last activity
//! from Cupola or the computer and none it opened is still open. The branch
//! its folder happens to be on never counts, and a set of pull requests that
//! could not be read whole decides nothing. Threads
//! that are pinned, working, running background work, waiting on an approval
//! or on the person at the computer, or holding a queued or unsettled send are
//! left alone. Each Project may override the computer-wide rule.
//!
//! Idle time never counts from before the rule turned on, so an upgrade or a
//! newly enabled rule archives nothing at once. A sweep archives a bounded
//! number of threads and asks GitHub about a bounded number of pull requests,
//! longest-unchecked first. A thread is judged first and archived only if
//! nothing changed meanwhile and the rule still makes it due (`commit`), so
//! new work and a changed setting always win.

use super::*;
use std::sync::LazyLock;

/// When each conversation was last checked for a merged pull request.
static MERGE_CHECKED: LazyLock<std::sync::Mutex<HashMap<String, i64>>> =
    LazyLock::new(Default::default);

/// The first sweep waits for the host to settle after starting.
const FIRST_SWEEP: Duration = Duration::from_secs(120);
const SWEEP_EVERY: Duration = Duration::from_secs(1_800);
/// Most threads one sweep archives, so a backlog drains over several sweeps.
const MAX_ARCHIVES_PER_SWEEP: usize = 25;
/// Most threads one sweep checks for a merged pull request; the threads
/// checked longest ago (or never) go first, so every thread gets its turn.
const MAX_MERGE_LOOKUPS: usize = 10;
/// Pages of `MAX_PROJECT_THREAD_PAGE` rows read per Project.
const MAX_PAGES: usize = 20;
/// A thread whose owner sent a message this recently is never archived.
const RECENT_SEND_SECONDS: i64 = 120;
const DAY_SECONDS: i64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    Idle,
    Merged,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::Idle => "idle",
            Reason::Merged => "merged",
        }
    }
}

/// What a saved conversation looked like when the sweep judged it; the
/// archive commits only while it still looks the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    last_activity_at: String,
    last_sent: Option<String>,
}

/// A sweep's judgement of one row, with the thread's newest activity from
/// any source (Unix seconds) it was judged on.
#[derive(Debug)]
pub(crate) enum Judged {
    /// Idle past the rule; `None` for a native thread Cupola does not keep yet.
    Idle(Option<Seen>, i64),
    /// Not idle, but archived if its pull request merged after the thread's
    /// newest activity, so a turn continued on the computer after the merge
    /// keeps it.
    MergeCandidate(Box<StoredProjectConversation>, Seen, i64),
}

/// What `commit` archives on: the judgement and why.
#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    seen: Option<Seen>,
    active: i64,
    reason: Reason,
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SWEEP).await;
        let mut interval = tokio::time::interval(SWEEP_EVERY);
        loop {
            interval.tick().await;
            sweep(&state, chrono::Utc::now().timestamp()).await;
        }
    })
}

/// One pass over every included Project at `now` (Unix seconds). Returns the
/// archived conversations and why.
pub(crate) async fn sweep(state: &AppState, now: i64) -> Vec<(String, Reason)> {
    let mut archived = Vec::new();
    // An update in progress owns the host; the next sweep catches up.
    if state.update_admission.claim_guard().await.is_none() {
        return archived;
    }
    let (Ok(host), Ok(projects)) = (
        state.store.auto_archive_rule().await,
        state.store.list_projects().await,
    ) else {
        return archived;
    };
    let Ok(approvals) = state.store.list_pending_approvals().await else {
        return archived;
    };
    let awaiting: HashSet<String> = approvals.into_iter().map(|a| a.thread_id).collect();
    let projects: Vec<_> = projects.into_iter().filter(|p| p.is_included).collect();
    let mut candidates = Vec::new();
    for project in &projects {
        let rule = project.auto_archive.clone().unwrap_or_else(|| host.clone());
        if rule.idle_days == 0 && !rule.on_merge {
            continue;
        }
        for row in project_rows(state, &project.id).await {
            if archived.len() >= MAX_ARCHIVES_PER_SWEEP {
                break;
            }
            match judge(state, &row, &rule, now, &awaiting).await {
                Some(Judged::Idle(seen, active)) => {
                    let verdict = Verdict {
                        seen,
                        active,
                        reason: Reason::Idle,
                    };
                    if let Some(id) = commit(state, project, &row, verdict, now).await {
                        archived.push((id, Reason::Idle));
                    }
                }
                Some(Judged::MergeCandidate(conversation, seen, active)) => {
                    candidates.push((project, row, conversation, seen, active));
                }
                None => {}
            }
        }
    }
    let picked = {
        let mut checked = MERGE_CHECKED.lock().unwrap_or_else(|e| e.into_inner());
        let ids = candidates
            .iter()
            .map(|(_, _, c, _, _)| c.conversation_id.clone())
            .collect();
        merge_turns(ids, &mut checked, now)
    };
    for (project, row, conversation, seen, active) in candidates
        .into_iter()
        .filter(|(_, _, c, _, _)| picked.contains(&c.conversation_id))
    {
        if archived.len() >= MAX_ARCHIVES_PER_SWEEP {
            break;
        }
        let Some(after) = activity_text(active) else {
            continue;
        };
        let merged = crate::pull_requests::merged_since(state, &conversation, Some(&after)).await;
        if merged != Some(true) {
            continue;
        }
        let verdict = Verdict {
            seen: Some(seen),
            active,
            reason: Reason::Merged,
        };
        if let Some(id) = commit(state, project, &row, verdict, now).await {
            archived.push((id, Reason::Merged));
        }
    }
    if !archived.is_empty() {
        // Phones refresh their Chats lists, as after any archive.
        super::announce_chats_changed(state).await;
    }
    archived
}

/// RFC 3339 text for Unix seconds, as pull request records compare it.
fn activity_text(at: i64) -> Option<String> {
    Some(chrono::DateTime::from_timestamp(at, 0)?.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Whether `rule` archives, for `reason`, a thread last active at `active`.
/// Idle time never counts from before the rule turned on; a rule without a
/// start (never written) counts from now, so nothing is idle.
fn due(rule: &wonder_store::AutoArchiveRule, active: i64, now: i64, reason: Reason) -> bool {
    match reason {
        Reason::Idle => {
            let since = rule.since.as_deref().map(activity_seconds).unwrap_or(now);
            rule.idle_days > 0 && now - active.max(since) >= rule.idle_days * DAY_SECONDS
        }
        Reason::Merged => rule.on_merge,
    }
}

/// Which merge candidates this sweep checks: those checked longest ago (or
/// never) first, at most `MAX_MERGE_LOOKUPS`, recorded as checked at `now`.
/// Threads that are no longer candidates are forgotten.
fn merge_turns(
    mut candidates: Vec<String>,
    checked: &mut HashMap<String, i64>,
    now: i64,
) -> HashSet<String> {
    checked.retain(|id, _| candidates.contains(id));
    candidates.sort_by_key(|id| (checked.get(id).copied().unwrap_or(i64::MIN), id.clone()));
    candidates.truncate(MAX_MERGE_LOOKUPS);
    for id in &candidates {
        checked.insert(id.clone(), now);
    }
    candidates.into_iter().collect()
}

/// Every row the Chats list shows for a Project, as the phone reads it.
async fn project_rows(state: &AppState, project: &str) -> Vec<ProjectThreadSummary> {
    let mut rows = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let query = ThreadsQuery {
            cursor: cursor.take(),
            limit: Some(MAX_PROJECT_THREAD_PAGE),
        };
        let Ok(page) = thread_page(state, project, query, MAX_PROJECT_THREAD_PAGE).await else {
            break;
        };
        rows.extend(page.threads);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    rows
}

/// Whether a row is idle past the rule or waits on a merge check, or `None`
/// while it must stay.
async fn judge(
    state: &AppState,
    row: &ProjectThreadSummary,
    rule: &wonder_store::AutoArchiveRule,
    now: i64,
    awaiting: &HashSet<String>,
) -> Option<Judged> {
    if held(row) || native_id(row).is_some_and(|id| awaiting.contains(id)) {
        return None;
    }
    let conversation = match &row.conversation_id {
        Some(id) => {
            let conversation = state.store.project_conversation(id).await.ok()??;
            if conversation.is_pinned
                || conversation.wonder_archived
                || state
                    .store
                    .conversation_has_archive_blocking_work(id)
                    .await
                    .unwrap_or(true)
            {
                return None;
            }
            if let Ok(Some(binding)) = state.store.runtime_binding(id).await {
                if awaiting.contains(&binding.thread_id) {
                    return None;
                }
            }
            Some(conversation)
        }
        None => None,
    };
    let last_sent = match &conversation {
        Some(c) => state
            .store
            .last_sent_message_at(&c.conversation_id)
            .await
            .ok()?,
        None => None,
    };
    let last_sent_at = last_sent.as_deref().map(activity_seconds);
    if last_sent_at.is_some_and(|at| now - at < RECENT_SEND_SECONDS) {
        return None;
    }
    let active = [
        Some(row.updated_at),
        conversation
            .as_ref()
            .map(|c| activity_seconds(&c.last_activity_at)),
        last_sent_at,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(now);
    let seen = conversation.as_ref().map(|c| Seen {
        last_activity_at: c.last_activity_at.clone(),
        last_sent,
    });
    if due(rule, active, now, Reason::Idle) {
        return Some(Judged::Idle(seen, active));
    }
    // Only a thread Cupola keeps has pull request records to settle by.
    let conversation = conversation.filter(|_| due(rule, active, now, Reason::Merged))?;
    Some(Judged::MergeCandidate(
        Box::new(conversation),
        seen?,
        active,
    ))
}

/// Pinned, working, running background work or waiting on the computer.
fn held(row: &ProjectThreadSummary) -> bool {
    row.is_pinned
        || row.is_working
        || row.background_work
        || row.needs_input_on_mac
        || row.waiting_on_mac_permission
}

/// Archive a judged row unless anything changed since it was judged. The
/// provider's own state (a turn started on the computer, background work) is
/// read again; under the archive lock, the thread's newest activity on the
/// computer (a turn that ran and finished meanwhile) and the effective rule
/// (computer and Project) are read again; then, under the dispatch locks a
/// send takes, one guarded write hides the thread only if no message, pin,
/// approval or activity arrived. Archived by the same path as the phone's
/// Archive, which keeps the archive lock until the provider answers so a
/// Restore cannot slip in between; `None` when the thread stays.
pub(crate) async fn commit(
    state: &AppState,
    project: &StoredProject,
    row: &ProjectThreadSummary,
    verdict: Verdict,
    now: i64,
) -> Option<String> {
    let Verdict {
        seen,
        active,
        reason,
    } = verdict;
    let item = ArchiveBatchItem {
        reference: row.reference.clone(),
        conversation_id: row.conversation_id.clone(),
    };
    let skipped = |why: &str| {
        let _ = state.logger.record(
            "info",
            "project_auto_archive_skipped",
            json!({"reference": row.reference, "reason": why}),
        );
        None
    };
    let conversation = match archive_target(state, project, &item).await {
        Ok(conversation) => conversation,
        Err((_, message)) => return skipped(&message),
    };
    // A thread attached just now has nothing sent from Cupola yet.
    let seen = seen.unwrap_or_else(|| Seen {
        last_activity_at: conversation.last_activity_at.clone(),
        last_sent: None,
    });
    if still_working(state, &conversation).await {
        return skipped("working");
    }
    let id = conversation.conversation_id.clone();
    let mut threads = conversation
        .native_session_id
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Ok(Some(binding)) = state.store.runtime_binding(&id).await {
        threads.push(binding.thread_id);
    }
    let _archive = state.projects.archive_lock(&id).await;
    let _admission = state.update_admission.claim_guard().await?;
    match native_activity(state, &conversation).await {
        Ok(Some(at)) if at > active => return skipped("active"),
        Ok(_) => {}
        Err(_) => return skipped("unreadable"),
    }
    {
        // A send holds its chat's lock until its provider answers.
        let Ok(_sending) =
            tokio::time::timeout(Duration::ZERO, state.projects.conversation_dispatch(&id)).await
        else {
            return skipped("sending");
        };
        let _dispatch = state.dispatch_lock.lock().await;
        if !effective_rule(state, &project.id)
            .await
            .is_some_and(|rule| due(&rule, active, now, reason))
        {
            return skipped("rule");
        }
        let archived = state
            .store
            .auto_archive_project_conversation(
                &id,
                &seen.last_activity_at,
                seen.last_sent.as_deref(),
                &threads,
                &now_text(),
            )
            .await;
        if !matches!(archived, Ok(true)) {
            return skipped("changed");
        }
    }
    reconcile_archive_cursors(state, &conversation, true);
    archive_in_provider(state, &conversation).await;
    let _ = state.logger.record(
        "info",
        "project_auto_archived",
        json!({"conversationId": id, "reason": reason.as_str()}),
    );
    Some(id)
}

/// The rule a Project's threads follow now: its own, else the computer's;
/// `None` when the Project is gone, left out or unreadable.
async fn effective_rule(state: &AppState, project: &str) -> Option<wonder_store::AutoArchiveRule> {
    let host = state.store.auto_archive_rule().await.ok()?;
    let project = state.store.project(project).await.ok()??;
    project
        .is_included
        .then(|| project.auto_archive.unwrap_or(host))
}

/// The thread's newest activity on the computer (Unix seconds), read fresh
/// from the source the Chats list reads: Codex's thread index, or the
/// modification time of Claude Code's transcript. `Err` for legacy homes
/// or when the provider cannot say.
async fn native_activity(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<Option<i64>, String> {
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return Err(crate::providers::other_history(conversation.family));
    }
    let Some(native) = conversation.native_session_id.as_deref() else {
        return Ok(None);
    };
    match conversation.family {
        AgentFamily::Codex => {
            // Legacy stores are refused without starting another runtime.
            let rpc = crate::providers::rpc_for_store(
                state,
                conversation.family,
                &conversation.provider_store,
            )
            .await?;
            let read = result(&rpc, "thread/read", json!({"threadId": native})).await?;
            Ok(read.pointer("/thread/updatedAt").and_then(Value::as_i64))
        }
        AgentFamily::Claude => {
            let Some(path) =
                crate::desktop_activity::claude_session_transcript(native, &conversation.cwd)
            else {
                return Ok(None);
            };
            Ok(tokio::fs::metadata(path)
                .await
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|at| at.as_secs() as i64))
        }
        AgentFamily::OpenCode | AgentFamily::Cursor => Ok(None),
    }
}

/// Whether the provider reports the thread working or running background
/// work now, read fresh as the Chats list reads it.
async fn still_working(state: &AppState, conversation: &StoredProjectConversation) -> bool {
    let mut row = attached_summary(
        state,
        conversation,
        activity_seconds(&conversation.last_activity_at),
        None,
    )
    .await;
    let (desktop, _, threads) =
        claude_activity(state, claude_session_ids([&row].into_iter())).await;
    mark_desktop_working(state, &mut row, &desktop).await;
    mark_background_work(state, &mut [&mut row], &desktop, threads.as_ref()).await;
    held(&row)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{call, fixture};

    fn row(reference: &str, updated_at: i64) -> ProjectThreadSummary {
        ProjectThreadSummary {
            reference: reference.into(),
            conversation_id: None,
            title: "Thread".into(),
            family: AgentFamily::Codex,
            updated_at,
            is_pinned: false,
            has_unread: false,
            is_working: false,
            is_computer_session: false,
            worktree_label: None,
            current_activity: None,
            needs_input_on_mac: false,
            waiting_on_mac_permission: false,
            branch: None,
            background_work: false,
            is_subagent: false,
            last_automation_at: None,
            automation_run: None,
            resumes_at: None,
            cwd: None,
        }
    }

    async fn draft(state: &AppState, project: &str, id: &str, cwd: &str, now: &str) {
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: id,
                project_id: project,
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd,
                roots_revision: 1,
                title: id,
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now,
            })
            .await
            .unwrap();
    }

    // Contract: a row the rule would archive stays while anything still needs
    // it: a pin, a running turn, background work, an approval or a question
    // at the computer.
    #[tokio::test]
    async fn rows_that_still_need_attention_are_never_due() {
        let (_dir, state) = fixture().await;
        let rule = wonder_store::AutoArchiveRule {
            idle_days: 3,
            on_merge: true,
            since: activity_text(0),
        };
        let now = 10 * DAY_SECONDS;
        let awaiting = HashSet::from(["asking".to_owned()]);
        let idle = row("codex:idle", now - 4 * DAY_SECONDS);
        assert!(matches!(
            judge(&state, &idle, &rule, now, &awaiting).await,
            Some(Judged::Idle(None, _))
        ));
        let recent = row("codex:recent", now - 2 * DAY_SECONDS);
        assert!(judge(&state, &recent, &rule, now, &awaiting)
            .await
            .is_none());
        // Idle time never counts from before the rule turned on.
        let turned_on = wonder_store::AutoArchiveRule {
            since: activity_text(now - DAY_SECONDS),
            ..rule.clone()
        };
        assert!(judge(&state, &idle, &turned_on, now, &awaiting)
            .await
            .is_none());
        type Hold = fn(&mut ProjectThreadSummary);
        let exempt: [(&str, Hold); 6] = [
            ("pinned", |r| r.is_pinned = true),
            ("working", |r| r.is_working = true),
            ("background work", |r| r.background_work = true),
            ("question on the computer", |r| r.needs_input_on_mac = true),
            ("permission dialog", |r| r.waiting_on_mac_permission = true),
            ("pending approval", |r| r.reference = "codex:asking".into()),
        ];
        for (case, change) in exempt {
            let mut held = idle.clone();
            change(&mut held);
            assert!(
                judge(&state, &held, &rule, now, &awaiting).await.is_none(),
                "{case}"
            );
        }
        // A thread Cupola does not keep has no pull requests to check.
        assert!(judge(&state, &recent, &rule, now, &awaiting)
            .await
            .is_none());
    }

    // Contract: the sweep archives idle Project threads for real (gone from
    // the list, archived in the store), keeps pinned and queued ones, honours
    // a Project's own rule, and never archives at once when a rule turns on.
    #[tokio::test]
    async fn sweep_archives_idle_threads_and_honours_rules() {
        let (dir, state) = fixture().await;
        let folder = |name: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            crate::canonical_path::canonicalize(path).unwrap()
        };
        let (app, other) = (folder("app"), folder("other"));
        for (id, path) in [("app", &app), ("other", &other)] {
            let roots = validate_folders(&[path.to_str().unwrap().into()], &[]).unwrap();
            state
                .store
                .create_project(id, id, "hash", id, &roots, 0, "now")
                .await
                .unwrap();
        }
        let start = chrono::Utc::now();
        let created = start.to_rfc3339_opts(SecondsFormat::Millis, true);
        let (app, other) = (app.to_str().unwrap(), other.to_str().unwrap());
        for id in ["idle", "pinned", "queued"] {
            draft(&state, "app", id, app, &created).await;
        }
        draft(&state, "other", "own-rule", other, &created).await;
        state
            .store
            .update_project_conversation(
                "pinned",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                &created,
            )
            .await
            .unwrap();
        state
            .store
            .insert_message("owner", "queued-1", "next", "hash", "queued", &created)
            .await
            .unwrap();
        // The other Project turns idle archiving off.
        let (status, project) = call(
            &state,
            "PATCH",
            "/api/v1/projects/other",
            json!({"autoArchive": {"idleDays": 0, "onMerge": false}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{project}");
        crate::test_support::assert_http_contract("projectSummary", &project);
        assert_eq!(
            project["autoArchive"],
            json!({"idleDays": 0, "onMerge": false})
        );

        let day = |days: f64| start.timestamp() + (days * DAY_SECONDS as f64) as i64;
        // The default rule waits three idle days.
        assert!(sweep(&state, day(1.0)).await.is_empty());
        // The fake runtime's own native thread is idle too; it is attached
        // and archived like any other row.
        let mine = |archived: Vec<(String, Reason)>| {
            let mut mine: Vec<_> = archived
                .into_iter()
                .filter(|(id, _)| id.len() < 36)
                .collect();
            mine.sort();
            mine
        };
        assert_eq!(
            mine(sweep(&state, day(4.0)).await),
            vec![("idle".to_owned(), Reason::Idle)]
        );
        let stored = |id: &'static str| {
            let state = state.clone();
            async move {
                state
                    .store
                    .project_conversation(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .wonder_archived
            }
        };
        assert!(stored("idle").await);
        for kept in ["pinned", "queued", "own-rule"] {
            assert!(!stored(kept).await, "{kept}");
        }
        let (_, page) = call(&state, "GET", "/api/v1/projects/app/threads", json!({})).await;
        let listed: Vec<_> = page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["conversationId"].clone())
            .collect();
        assert!(!listed.contains(&json!("idle")), "{page}");
        assert!(listed.contains(&json!("pinned")), "{page}");

        // Back on the computer's rule, the Project's thread is archived; a
        // rule turned off and on again counts idle time from then.
        let (status, project) = call(
            &state,
            "PATCH",
            "/api/v1/projects/other",
            json!({"autoArchive": null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{project}");
        assert!(project.get("autoArchive").is_none(), "{project}");
        draft(&state, "app", "later", app, &created).await;
        let (status, saved) = call(
            &state,
            "PUT",
            "/api/v1/settings/projects",
            json!({"autoArchive": {"idleDays": 0, "onMerge": false}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        crate::test_support::assert_http_contract("projectDiscoverySettings", &saved);
        assert_eq!(saved["includeWorktrees"], true);
        assert!(
            sweep(&state, day(10.0)).await.is_empty(),
            "off archives nothing"
        );
        let turned_on = chrono::DateTime::from_timestamp(day(9.0), 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        state
            .store
            .set_auto_archive_rule(3, true, &turned_on)
            .await
            .unwrap();
        assert!(
            sweep(&state, day(10.0)).await.is_empty(),
            "one day since on"
        );
        assert_eq!(
            mine(sweep(&state, day(12.5)).await),
            vec![
                ("later".to_owned(), Reason::Idle),
                ("own-rule".to_owned(), Reason::Idle)
            ]
        );
        let (status, _) = call(
            &state,
            "PUT",
            "/api/v1/settings/projects",
            json!({"autoArchive": {"idleDays": 91, "onMerge": false}}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        state.projects.shutdown().await;
    }

    // Contract: every thread waiting on a merge check gets one. Each sweep
    // checks the threads checked longest ago (or never) first, so threads
    // later in the list are not starved by the first ten.
    #[test]
    fn merge_checks_rotate_through_every_thread() {
        let ids: Vec<String> = (0..25).map(|n| format!("thread-{n:02}")).collect();
        let mut checked = HashMap::new();
        let mut seen = HashSet::new();
        for sweep in 0..3 {
            let picked = merge_turns(ids.clone(), &mut checked, sweep * 1_800);
            assert_eq!(picked.len(), MAX_MERGE_LOOKUPS);
            if sweep < 2 {
                assert!(picked.is_disjoint(&seen), "sweep {sweep} repeated a thread");
            }
            seen.extend(picked);
        }
        assert_eq!(
            seen.len(),
            ids.len(),
            "every thread checked within three sweeps"
        );
        // A thread that stops waiting is forgotten.
        merge_turns(ids[..3].to_vec(), &mut checked, 9_000);
        assert_eq!(checked.len(), 3);
    }

    // Contract: a thread judged idle stays if a send or a pin arrives before
    // the archive commits (as during a slow pull request lookup); its queued
    // send is not interrupted. Once quiet again, it archives.
    #[tokio::test]
    async fn arrivals_after_judgement_keep_the_thread() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("app");
        std::fs::create_dir_all(&path).unwrap();
        let path = crate::canonical_path::canonicalize(path).unwrap();
        let roots = validate_folders(&[path.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        let project = state.store.project("app").await.unwrap().unwrap();
        let start = chrono::Utc::now();
        let created = start.to_rfc3339_opts(SecondsFormat::Millis, true);
        let rule = state
            .store
            .set_auto_archive_rule(3, false, &created)
            .await
            .unwrap();
        let later = start.timestamp() + 4 * DAY_SECONDS;
        let stored = |id: &'static str| {
            let state = state.clone();
            async move { state.store.project_conversation(id).await.unwrap().unwrap() }
        };
        let judged = |id: &'static str| {
            let state = state.clone();
            let rule = rule.clone();
            async move {
                let conversation = state.store.project_conversation(id).await.unwrap().unwrap();
                let mut row = attached_summary(&state, &conversation, 0, None).await;
                row.updated_at = activity_seconds(&conversation.last_activity_at);
                match judge(&state, &row, &rule, later, &HashSet::new()).await {
                    Some(Judged::Idle(seen @ Some(_), active)) => (
                        row,
                        Verdict {
                            seen,
                            active,
                            reason: Reason::Idle,
                        },
                    ),
                    other => panic!("{id} not idle: {other:?}"),
                }
            }
        };

        draft(&state, "app", "sent", path.to_str().unwrap(), &created).await;
        let (row, verdict) = judged("sent").await;
        let Ok(MessageInsert::Inserted(arrived)) = state
            .store
            .insert_message("owner", "arrived", "one more", "hash", "sent", &created)
            .await
        else {
            panic!("send not saved");
        };
        assert!(commit(&state, &project, &row, verdict, later)
            .await
            .is_none());
        assert!(!stored("sent").await.wonder_archived);
        let message = state
            .store
            .message_by_id(&arrived.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            message.state, arrived.state,
            "the arriving send was settled"
        );

        draft(&state, "app", "pinned", path.to_str().unwrap(), &created).await;
        let (row, verdict) = judged("pinned").await;
        state
            .store
            .update_project_conversation(
                "pinned",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                &created,
            )
            .await
            .unwrap();
        assert!(commit(&state, &project, &row, verdict, later)
            .await
            .is_none());
        assert!(!stored("pinned").await.wonder_archived);

        draft(&state, "app", "quiet", path.to_str().unwrap(), &created).await;
        let (row, verdict) = judged("quiet").await;
        assert_eq!(
            commit(&state, &project, &row, verdict, later).await,
            Some("quiet".to_owned())
        );
        assert!(stored("quiet").await.wonder_archived);
        state.projects.shutdown().await;
    }

    // Contract: a setting saved between judgement and archive wins: a rule
    // turned off or raised on the computer, or turned off for the Project,
    // keeps the thread; once the rule makes it due again, it archives.
    #[tokio::test]
    async fn a_rule_changed_before_the_archive_keeps_the_thread() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("app");
        std::fs::create_dir_all(&path).unwrap();
        let path = crate::canonical_path::canonicalize(path).unwrap();
        let roots = validate_folders(&[path.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        let start = chrono::Utc::now();
        let created = start.to_rfc3339_opts(SecondsFormat::Millis, true);
        let rule = state
            .store
            .set_auto_archive_rule(3, false, &created)
            .await
            .unwrap();
        draft(&state, "app", "quiet", path.to_str().unwrap(), &created).await;
        let conversation = state
            .store
            .project_conversation("quiet")
            .await
            .unwrap()
            .unwrap();
        let mut row = attached_summary(&state, &conversation, 0, None).await;
        row.updated_at = activity_seconds(&conversation.last_activity_at);
        let later = start.timestamp() + 4 * DAY_SECONDS;
        let Some(Judged::Idle(seen, active)) =
            judge(&state, &row, &rule, later, &HashSet::new()).await
        else {
            panic!("not idle");
        };
        let verdict = Verdict {
            seen,
            active,
            reason: Reason::Idle,
        };
        let save = |path: &'static str, method: &'static str, rule: Value| {
            let state = state.clone();
            async move {
                let (status, body) = call(&state, method, path, json!({"autoArchive": rule})).await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
        };
        let archive = || {
            let (state, row, verdict) = (state.clone(), row.clone(), verdict.clone());
            async move {
                let project = state.store.project("app").await.unwrap().unwrap();
                commit(&state, &project, &row, verdict, later).await
            }
        };
        let settings = "/api/v1/settings/projects";
        save(settings, "PUT", json!({"idleDays": 30, "onMerge": false})).await;
        assert_eq!(archive().await, None, "raised to 30 days");
        save(settings, "PUT", json!({"idleDays": 0, "onMerge": false})).await;
        assert_eq!(archive().await, None, "turned off");
        save(settings, "PUT", json!({"idleDays": 3, "onMerge": false})).await;
        let own = "/api/v1/projects/app";
        save(own, "PATCH", json!({"idleDays": 0, "onMerge": false})).await;
        assert_eq!(archive().await, None, "off for the Project");
        assert!(
            !state
                .store
                .project_conversation("quiet")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        save(own, "PATCH", Value::Null).await;
        assert_eq!(archive().await, Some("quiet".to_owned()));
        state.projects.shutdown().await;
    }

    // Contract: a rule save, for the computer or a Project, waits while an
    // archive holds the dispatch lock between reading the rule and archiving.
    #[tokio::test]
    async fn rule_saves_wait_for_an_archive_in_progress() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("app");
        std::fs::create_dir_all(&path).unwrap();
        let roots = validate_folders(&[path.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        for (method, path) in [
            ("PUT", "/api/v1/settings/projects"),
            ("PATCH", "/api/v1/projects/app"),
        ] {
            let committing = state.dispatch_lock.lock().await;
            let save = tokio::spawn({
                let state = state.clone();
                async move {
                    call(
                        &state,
                        method,
                        path,
                        json!({"autoArchive": {"idleDays": 0, "onMerge": false}}),
                    )
                    .await
                }
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!save.is_finished(), "{path} saved during an archive");
            drop(committing);
            let (status, body) = save.await.unwrap();
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        state.projects.shutdown().await;
    }

    // Contract: a turn that ran and finished on the computer while the sweep
    // asked GitHub about the merge keeps the thread, even though nothing in
    // Cupola changed; judged after that turn, it archives.
    #[tokio::test]
    async fn a_desktop_turn_during_the_merge_lookup_keeps_the_thread() {
        let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", None, None)
            .await
            .unwrap();
        let project = state.store.project("project").await.unwrap().unwrap();
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        let judged_at = 1_000;
        let native = |updated_at: i64| {
            std::fs::write(
                dir.path().join("idle-fixture.json"),
                json!({"thread": {
                    "id": "thread", "cwd": conversation.cwd, "name": "Recovery",
                    "updatedAt": updated_at, "status": {"type": "idle"}, "parentThreadId": null,
                }})
                .to_string(),
            )
            .unwrap();
        };
        let row = attached_summary(&state, &conversation, judged_at, None).await;
        let last_sent = state
            .store
            .last_sent_message_at("project-chat")
            .await
            .unwrap();
        // The default rule archives on a merge.
        let verdict = |active: i64| Verdict {
            seen: Some(Seen {
                last_activity_at: conversation.last_activity_at.clone(),
                last_sent: last_sent.clone(),
            }),
            active,
            reason: Reason::Merged,
        };
        // Judged at 1 000; the desktop turn finished at 4 600.
        native(judged_at + 3_600);
        assert_eq!(
            commit(&state, &project, &row, verdict(judged_at), DAY_SECONDS).await,
            None
        );
        assert!(
            !state
                .store
                .project_conversation("project-chat")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        assert_eq!(
            commit(
                &state,
                &project,
                &row,
                verdict(judged_at + 3_600),
                DAY_SECONDS
            )
            .await,
            Some("project-chat".to_owned())
        );
        state.projects.shutdown().await;
    }

    // Contract: a merge settles a thread only if it came after the thread's
    // newest activity from any source; a turn continued on the computer after
    // the merge (newer than Cupola's last message) moves the bar past it.
    #[tokio::test]
    async fn merges_are_judged_against_activity_on_the_computer() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("app");
        std::fs::create_dir_all(&path).unwrap();
        let path = crate::canonical_path::canonicalize(path).unwrap();
        let roots = validate_folders(&[path.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        let start = chrono::Utc::now().timestamp();
        let text = |at: i64| {
            chrono::DateTime::from_timestamp(at, 0)
                .unwrap()
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        draft(
            &state,
            "app",
            "merged",
            path.to_str().unwrap(),
            &text(start),
        )
        .await;
        let Ok(MessageInsert::Inserted(sent)) = state
            .store
            .insert_message("owner", "sent", "ship it", "hash", "merged", &text(start))
            .await
        else {
            panic!("send not saved");
        };
        state
            .store
            .update_message_delivery(&sent.id, "completed", None, None)
            .await
            .unwrap();
        let rule = wonder_store::AutoArchiveRule {
            idle_days: 3,
            on_merge: true,
            since: None,
        };
        let conversation = state
            .store
            .project_conversation("merged")
            .await
            .unwrap()
            .unwrap();
        let mut row = attached_summary(&state, &conversation, start, None).await;
        let now = start + DAY_SECONDS;
        let after = |row: ProjectThreadSummary| {
            let state = state.clone();
            let rule = rule.clone();
            async move {
                match judge(&state, &row, &rule, now, &HashSet::new()).await {
                    Some(Judged::MergeCandidate(_, _, active)) => activity_text(active).unwrap(),
                    other => panic!("not a merge candidate: {other:?}"),
                }
            }
        };
        assert_eq!(after(row.clone()).await, text(start));
        // The owner continued on the computer an hour after the merge.
        let desktop = start + 3 * 3_600;
        row.updated_at = desktop;
        assert_eq!(after(row).await, text(desktop));
        // Legacy history must not be judged using the computer's own native
        // activity (or its absence), for either formerly supported provider.
        let mut legacy = conversation.clone();
        legacy.native_session_id = Some("legacy-session".into());
        legacy.provider_store = "retired-home".into();
        for family in [AgentFamily::Codex, AgentFamily::Claude] {
            legacy.family = family;
            assert!(native_activity(&state, &legacy).await.is_err());
        }
        state.projects.shutdown().await;
    }

    // Contract: auto-archive keeps the archive lock until Codex answers, so a
    // Restore that arrives meanwhile runs after it and leaves the thread
    // unarchived in both Cupola and Codex.
    #[tokio::test]
    async fn restore_during_a_slow_codex_archive_wins() {
        let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", None, None)
            .await
            .unwrap();
        let project = state.store.project("project").await.unwrap().unwrap();
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {
                "id": "thread", "cwd": conversation.cwd, "name": "Recovery", "updatedAt": 1,
                "status": {"type": "idle"}, "parentThreadId": null,
            }})
            .to_string(),
        )
        .unwrap();
        let row = attached_summary(&state, &conversation, 0, None).await;
        // The default rule archives on a merge.
        let verdict = Verdict {
            seen: Some(Seen {
                last_activity_at: conversation.last_activity_at.clone(),
                last_sent: state
                    .store
                    .last_sent_message_at("project-chat")
                    .await
                    .unwrap(),
            }),
            active: 1,
            reason: Reason::Merged,
        };
        std::fs::write(dir.path().join("delay-archive"), "").unwrap();
        let archiving = {
            let (state, project, row) = (state.clone(), project.clone(), row.clone());
            tokio::spawn(async move { commit(&state, &project, &row, verdict, DAY_SECONDS).await })
        };
        let waiting = dir.path().join("archive-waiting");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !waiting.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Codex was asked to archive");
        let restoring = {
            let state = state.clone();
            tokio::spawn(async move {
                let current = state
                    .store
                    .project_conversation("project-chat")
                    .await
                    .unwrap()
                    .unwrap();
                set_archived(&state, &current, false).await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!restoring.is_finished(), "Restore waits for the archive");
        std::fs::remove_file(dir.path().join("delay-archive")).unwrap();
        assert_eq!(archiving.await.unwrap(), Some("project-chat".to_owned()));
        restoring.await.unwrap().unwrap();
        assert!(
            !dir.path().join("archived-thread").exists(),
            "Codex left archived"
        );
        assert!(
            !state
                .store
                .project_conversation("project-chat")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        state.projects.shutdown().await;
    }

    // Contract: a Project save that changes folders and the archive rule
    // together is all or nothing; a stale folder revision leaves the rule.
    #[tokio::test]
    async fn refused_folder_change_leaves_the_rule() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("app");
        std::fs::create_dir_all(&path).unwrap();
        let path = crate::canonical_path::canonicalize(path).unwrap();
        let folder = path.to_str().unwrap();
        let roots = validate_folders(&[folder.into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        let save = |revision: i64, days: i64| {
            let state = state.clone();
            let body = json!({
                "folders": [folder],
                "rootsRevision": revision,
                "autoArchive": {"idleDays": days, "onMerge": false},
            });
            async move { call(&state, "PATCH", "/api/v1/projects/app", body).await }
        };
        let (status, body) = save(99, 7).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let project = state.store.project("app").await.unwrap().unwrap();
        assert_eq!(project.auto_archive, None, "rule saved by a refused save");
        let (status, body) = save(project.roots_revision, 7).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["autoArchive"],
            json!({"idleDays": 7, "onMerge": false})
        );
        state.projects.shutdown().await;
    }
}
