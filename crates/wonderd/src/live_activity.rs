//! Live Activity and Dynamic Island, reported by this Mac.
//!
//! The phone shows ONE Live Activity for all its computers. Each computer
//! reports only its own chats (one row per chat) to the push Worker, which keeps
//! the rows per registration, merges every computer's rows into one state and
//! pushes it to the phone's card (`services/push/src/activity.ts`). This loop
//! reads the host's own status (Wonder-owned turns, which end only when the
//! harness confirms them, pending approvals and questions, and work in the Codex
//! and Claude apps that `external_push` watches) and reports every change, so a
//! finished or stopped chat leaves the Lock Screen while Wonder is suspended.
//!
//! Live Activity content is rendered by iOS itself; no extension can decrypt it.
//! A row is a truncated chat title and Project name (the owner allows them in the
//! push, and the Worker keeps them only briefly), a status, a start time and a
//! random route per device. Never a provider, message, command, file name or
//! conversation ID. The activity is status only: it has no buttons, and tapping a
//! row opens the chat its route names. The phone's tokens and whether the owner
//! dismissed the card live in the Worker; this Mac only forwards the phone's word
//! about them (`register`).
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

/// Working chats are reported again this often so the Worker's running rows
/// (which expire after two hours) and the card never go stale.
pub(crate) const HEARTBEAT_SECONDS: i64 = 4 * 60;
/// Rows reported to the Worker; the Worker shows five and counts the rest.
pub(crate) const ROW_LIMIT: usize = 20;
/// A finished chat's row is reported for this long, so a report that failed
/// once still lands. The Worker keeps a finished row 15 minutes from its first report.
pub(crate) const ROW_FINISHED_SECONDS: i64 = 120;
/// A phone with no Live Activity token yet stores no rows: ask again after this.
const UNSTORED_RETRY_SECONDS: i64 = 60;
/// Titles and Project names are cut to these lengths in the push.
const TITLE_CHARS: usize = 40;
const PROJECT_CHARS: usize = 24;
/// The loop's backstop; host events and the desktop watcher wake it sooner.
const TICK: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Phase {
    Running,
    NeedsApproval,
    NeedsInput,
    Done,
    Failed,
    Stopped,
}

impl Phase {
    pub(crate) fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Stopped)
    }
    /// The outcome shown when several threads ended: a failure wins, then a
    /// stop, then done.
    fn worst(self, other: Self) -> Self {
        let rank = |phase| match phase {
            Self::Failed => 3,
            Self::Stopped => 2,
            _ => 1,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
    /// A Wonder-owned turn's final message state.
    fn from_message_state(state: Option<&str>) -> Self {
        match state {
            // `uncertain`: the turn ended with an outcome Wonder cannot read.
            Some("failed" | "uncertain") => Self::Failed,
            Some("interrupted") => Self::Stopped,
            _ => Self::Done,
        }
    }
}

/// One thread's row in the activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    /// Never sent; each device gets a random route.
    pub thread: String,
    pub status: Phase,
    /// When the thread began work (or, for a finished row, ended).
    pub since: i64,
    pub title: String,
    pub project: String,
}

impl Phase {
    /// Rows list what needs the owner first, then work, then endings.
    fn row_rank(self) -> u8 {
        match self {
            Self::NeedsApproval => 0,
            Self::NeedsInput => 1,
            Self::Running => 2,
            Self::Failed => 3,
            Self::Stopped => 4,
            Self::Done => 5,
        }
    }
}

/// Work in another app on the Mac, as the desktop watcher last read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Desktop {
    Working,
    /// Waiting on the owner at the Mac (a permission prompt or question).
    Waiting,
}

/// Status shared between the desktop watcher, this loop and the HTTP handler.
#[derive(Default)]
pub struct Hub {
    /// Callback boundaries for the real-loop fixture; absent from shipped builds.
    #[cfg(test)]
    milestones: std::sync::Mutex<Vec<(&'static str, std::time::Instant)>>,
    desktop: std::sync::Mutex<HashMap<String, Desktop>>,
    /// Threads kept for one reading after they vanished without an ending
    /// (an unreadable record is not an exit).
    held: std::sync::Mutex<BTreeSet<String>>,
    /// How desktop work ended since the loop last read it.
    desktop_ended: std::sync::Mutex<HashMap<String, Phase>>,
    current: std::sync::Mutex<Option<Aggregate>>,
    /// What each registration's Worker last accepted from this Mac.
    reported: std::sync::Mutex<HashMap<String, Reported>>,
    wake: tokio::sync::Notify,
}

#[derive(Clone, Debug, Default)]
struct Reported {
    encoded: String,
    at: i64,
    next_attempt: i64,
    failures: u32,
    /// The Worker asked for another report then (a push-start waiting out the phone's own card).
    revisit_at: Option<i64>,
}

impl Hub {
    pub(crate) fn notification_policy_changed(&self) {
        self.wake.notify_one();
    }

    /// The watcher's full reading of followed threads, with how any that
    /// stopped working ended. A thread that vanished without an ending stays
    /// for one more reading, so one unreadable record does not end the work.
    pub(crate) fn desktop_reading(
        &self,
        mut reading: HashMap<String, Desktop>,
        ended: Vec<(String, Phase)>,
    ) {
        let changed = {
            let mut desktop = self.desktop.lock().unwrap();
            let mut held = self.held.lock().unwrap();
            let mut next_held = BTreeSet::new();
            for (id, state) in desktop.iter() {
                let explained = ended.iter().any(|(ended, _)| ended == id);
                if !reading.contains_key(id) && !explained && !held.contains(id) {
                    reading.insert(id.clone(), *state);
                    next_held.insert(id.clone());
                }
            }
            *held = next_held;
            let changed = *desktop != reading;
            *desktop = reading;
            changed
        };
        if !ended.is_empty() {
            self.desktop_ended.lock().unwrap().extend(ended);
        }
        if changed {
            self.wake.notify_one();
        }
    }

    /// The host-wide state the status truth table reads (its tests are unix-only).
    #[cfg(all(test, unix))]
    pub(crate) fn current(&self) -> Option<Aggregate> {
        self.current.lock().unwrap().clone()
    }
}

/// The one activity a phone shows for this Mac: the most urgent state across
/// every thread with work, and how many there are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Aggregate {
    pub phase: Phase,
    /// Threads with agent work: running, waiting on approval or input.
    pub running: u32,
    /// Active parent conversations waiting on approval or input, counted once.
    pub needs_input: u32,
    /// Internal push-start grace/deduplication boundary, never displayed or sent.
    pub started_at: i64,
    /// The thread a tap opens: the one needing the owner, or the only
    /// thread with work. Never sent; each device gets a random route.
    pub target: Option<String>,
    /// Up to `ROW_LIMIT` threads, most urgent first.
    pub rows: Vec<Row>,
    /// Threads beyond the rows.
    pub more: u32,
}

/// What one evaluation reads from the host.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    /// Wonder-owned turns in progress (top-level conversations).
    pub owned: BTreeSet<String>,
    pub muted: BTreeSet<String>,
    pub muted_activity: bool,
    pub desktop: HashMap<String, Desktop>,
    /// Pending approvals' conversations (`""` before one is routed).
    pub approvals: Vec<String>,
    pub questions: BTreeSet<String>,
}

/// The loop's memory between evaluations.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    active: BTreeSet<String>,
    participants: BTreeSet<String>,
    owned: BTreeSet<String>,
    started_at: Option<i64>,
    outcomes: HashMap<String, Phase>,
    finished: Option<Aggregate>,
    /// When each active thread began work.
    row_since: HashMap<String, i64>,
    /// Threads that just ended, with how and when, until their row drops.
    recent: HashMap<String, (Phase, i64)>,
    /// Before anything ran since the Mac started: how the newest Wonder-owned
    /// turn ended (after a restart, a turn the restart failed reads failed).
    pub(crate) initial: Option<Phase>,
}

impl Tracker {
    /// The next aggregate. `outcome` says how a thread that left the active
    /// set ended (owned turns from their message, desktop work from the
    /// watcher).
    pub(crate) fn step(
        &mut self,
        inputs: &Inputs,
        now: i64,
        outcome: impl Fn(&str, bool) -> Phase,
    ) -> Aggregate {
        let working: BTreeSet<String> = inputs
            .owned
            .iter()
            .cloned()
            .chain(
                inputs
                    .desktop
                    .iter()
                    .filter(|(_, state)| **state == Desktop::Working)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        let waiting: BTreeSet<String> = inputs
            .questions
            .iter()
            .cloned()
            .chain(
                inputs
                    .desktop
                    .iter()
                    .filter(|(_, state)| **state == Desktop::Waiting)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        let approvals: BTreeSet<String> = inputs
            .approvals
            .iter()
            .filter(|id| !id.is_empty())
            .cloned()
            .collect();
        let active: BTreeSet<String> = working
            .iter()
            .chain(&waiting)
            .chain(&approvals)
            .cloned()
            .collect();
        for left in self.active.difference(&active) {
            let ended = outcome(left, self.owned.contains(left));
            self.outcomes
                .entry(left.clone())
                .and_modify(|seen| *seen = seen.worst(ended))
                .or_insert(ended);
            self.recent.insert(left.clone(), (ended, now));
        }
        self.row_since.retain(|id, _| active.contains(id));
        for id in &active {
            self.row_since.entry(id.clone()).or_insert(now);
            self.recent.remove(id);
        }
        self.recent
            .retain(|_, (_, at)| now - *at < ROW_FINISHED_SECONDS);
        if self.started_at.is_none() && !active.is_empty() {
            self.participants.clear();
        }
        self.participants.extend(active.iter().cloned());
        self.active = active.clone();
        self.owned = inputs.owned.clone();
        if active.is_empty() && inputs.approvals.is_empty() {
            if let Some(started_at) = self.started_at.take() {
                self.finished = Some(Aggregate {
                    phase: self
                        .outcomes
                        .values()
                        .copied()
                        .reduce(Phase::worst)
                        .unwrap_or(Phase::Done),
                    running: 0,
                    needs_input: 0,
                    started_at,
                    target: None,
                    ..self.rows(&BTreeMap::new())
                });
            }
            // Nothing ran since the Mac started: an activity a phone opens
            // now ends at once rather than claim work the Mac cannot see.
            return self
                .finished
                .get_or_insert_with(|| Aggregate {
                    phase: self.initial.unwrap_or(Phase::Done),
                    running: 0,
                    needs_input: 0,
                    started_at: now,
                    target: None,
                    rows: Vec::new(),
                    more: 0,
                })
                .clone();
        }
        if self.started_at.is_none() {
            self.outcomes.clear();
            self.finished = None;
        }
        let started_at = *self.started_at.get_or_insert(now);
        let statuses: BTreeMap<String, Phase> = active
            .iter()
            .map(|id| {
                let status = if approvals.contains(id) {
                    Phase::NeedsApproval
                } else if waiting.contains(id) {
                    Phase::NeedsInput
                } else {
                    Phase::Running
                };
                (id.clone(), status)
            })
            .collect();
        let (phase, target) = if !inputs.approvals.is_empty() {
            (Phase::NeedsApproval, approvals.iter().next().cloned())
        } else if !waiting.is_empty() {
            (Phase::NeedsInput, waiting.iter().next().cloned())
        } else {
            (Phase::Running, None)
        };
        let only = (active.len() == 1)
            .then(|| active.iter().next().cloned())
            .flatten();
        Aggregate {
            phase,
            running: active.len().max(1) as u32,
            // Waiting and approvals can refer to the same parent, including
            // several child tasks. Unknown approval routes preserve the existing
            // minimum of one agent without inventing another conversation.
            needs_input: waiting
                .union(&approvals)
                .count()
                .max(usize::from(!inputs.approvals.is_empty())) as u32,
            started_at,
            target: target.or(only),
            ..self.rows(&statuses)
        }
    }

    /// The rows for the active threads' statuses plus the recently finished
    /// ones, most urgent first and capped. Only `rows` and `more` are set.
    fn rows(&self, statuses: &BTreeMap<String, Phase>) -> Aggregate {
        let mut all: Vec<Row> = statuses
            .iter()
            .map(|(id, status)| Row {
                thread: id.clone(),
                status: *status,
                since: self.row_since.get(id).copied().unwrap_or_default(),
                title: String::new(),
                project: String::new(),
            })
            .chain(self.recent.iter().map(|(id, (status, at))| Row {
                thread: id.clone(),
                status: *status,
                since: *at,
                title: String::new(),
                project: String::new(),
            }))
            .collect();
        all.sort_by(|a, b| {
            (a.status.row_rank(), a.since, &a.thread).cmp(&(
                b.status.row_rank(),
                b.since,
                &b.thread,
            ))
        });
        let more = all.len().saturating_sub(ROW_LIMIT) as u32;
        all.truncate(ROW_LIMIT);
        Aggregate {
            phase: Phase::Done,
            running: 0,
            needs_input: 0,
            started_at: 0,
            target: None,
            rows: all,
            more,
        }
    }
}

/// One chat as reported: a short title and Project name, a status, a start
/// time and this phone's random route. No conversation ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportRow {
    route_id: String,
    title: String,
    project: String,
    status: Phase,
    started_at: i64,
}

/// Cut to `limit` characters on a character boundary, with control characters
/// and runs of whitespace flattened.
fn shorten(raw: &str, limit: usize) -> String {
    let flat = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut: String = flat.chars().take(limit.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) async fn read_inputs(state: &AppState) -> Option<Inputs> {
    #[cfg(test)]
    state
        .live_activity
        .milestones
        .lock()
        .unwrap()
        .push(("observation", std::time::Instant::now()));
    let mut visible = crate::projects::visible_conversations(state).await.ok()?;
    let muted: BTreeSet<String> = state
        .store
        .muted_notification_conversations()
        .await
        .ok()?
        .into_iter()
        .collect();
    visible.retain(|id| !muted.contains(id));
    let mut owned = state.store.owned_activity(now_ms() as i64).await.ok()?;
    let mut muted_activity = owned
        .running
        .iter()
        .chain(&owned.approvals)
        .any(|id| muted.contains(id));
    owned.running.retain(|id| visible.contains(id));
    // An approval not routed yet preserves the attention phase. Tracker
    // deliberately excludes that empty ID from the running-thread count.
    owned
        .approvals
        .retain(|id| id.is_empty() || visible.contains(id));
    // Questions stay stored in the child task that asked them. Resolve the
    // same verified Project ancestor as owned running turns before filtering.
    let mut questions = BTreeSet::new();
    for conversation in owned.questions {
        let parent = state
            .store
            .owning_project_conversation(&conversation)
            .await
            .ok()?
            .map(|parent| parent.conversation_id)
            .unwrap_or(conversation);
        muted_activity |= muted.contains(&parent);
        if visible.contains(&parent) {
            questions.insert(parent);
        }
    }
    let mut desktop = state.live_activity.desktop.lock().unwrap().clone();
    muted_activity |= desktop.keys().any(|id| muted.contains(id));
    // A turn Wonder owns is counted once, from its own record.
    desktop.retain(|id, _| visible.contains(id) && !owned.running.contains(id));
    Some(Inputs {
        muted_activity,
        muted,
        owned: owned.running.into_iter().collect(),
        desktop,
        approvals: owned.approvals,
        questions,
    })
}

/// How a thread that left the inputs ended. With no recorded outcome it is
/// `Stopped`: missing evidence never shows Done.
fn ended_phase(outcomes: &HashMap<String, Phase>, id: &str) -> Phase {
    outcomes.get(id).copied().unwrap_or(Phase::Stopped)
}

/// Read the host, step the aggregate and publish it to the hub.
async fn evaluate(state: &AppState, tracker: &mut Tracker, now: i64) -> Option<Aggregate> {
    let inputs = read_inputs(state).await?;
    // Muting is removal from the aggregate, not a successful/failed finish.
    tracker.active.retain(|id| !inputs.muted.contains(id));
    tracker.owned.retain(|id| !inputs.muted.contains(id));
    let outcomes_before = tracker.outcomes.len();
    tracker.outcomes.retain(|id, _| !inputs.muted.contains(id));
    if tracker.outcomes.len() != outcomes_before {
        if let Some(finished) = tracker.finished.as_mut() {
            finished.phase = tracker
                .outcomes
                .values()
                .copied()
                .reduce(Phase::worst)
                .unwrap_or(Phase::Done);
        }
    }
    let participants_muted = !tracker.participants.is_empty()
        && tracker
            .participants
            .iter()
            .all(|id| inputs.muted.contains(id));
    let empty = inputs.owned.is_empty()
        && inputs.desktop.is_empty()
        && inputs.questions.is_empty()
        && inputs.approvals.is_empty();
    let latest_muted = if empty && tracker.participants.is_empty() && !inputs.muted.is_empty() {
        if let Some((id, _)) = state.store.newest_owned_outcome().await.ok()? {
            let owner = state
                .store
                .owning_project_conversation(&id)
                .await
                .ok()?
                .map(|owner| owner.conversation_id)
                .unwrap_or(id);
            inputs.muted.contains(&owner)
        } else {
            false
        }
    } else {
        false
    };
    if empty
        && (inputs.muted_activity
            || participants_muted
            || (tracker.participants.is_empty() && latest_muted))
    {
        // Muted work gets no heartbeat or Done row, including after host
        // restart: report nothing, so rows the Worker holds for it are dropped.
        *state.live_activity.current.lock().unwrap() = None;
        report(state, &[], now).await;
        return None;
    }
    if participants_muted {
        *tracker = Tracker::default();
    }
    tracker.participants.retain(|id| !inputs.muted.contains(id));
    let left: Vec<String> = tracker
        .active
        .iter()
        .filter(|id| {
            !inputs.owned.contains(*id)
                && !inputs.desktop.contains_key(*id)
                && !inputs.questions.contains(*id)
                && !inputs.approvals.contains(id)
        })
        .cloned()
        .collect();
    let mut owned_outcomes = HashMap::new();
    for id in &left {
        if tracker.owned.contains(id) {
            let outcome = state.store.latest_owned_outcome(id).await.ok().flatten();
            owned_outcomes.insert(id.clone(), Phase::from_message_state(outcome.as_deref()));
        }
    }
    let desktop_ended = std::mem::take(&mut *state.live_activity.desktop_ended.lock().unwrap());
    let aggregate = tracker.step(&inputs, now, |id, owned| {
        if owned {
            ended_phase(&owned_outcomes, id)
        } else {
            ended_phase(&desktop_ended, id)
        }
    });
    let mut aggregate = aggregate;
    for row in &mut aggregate.rows {
        if let Ok(Some((title, project))) =
            state.store.live_activity_thread_label(&row.thread).await
        {
            row.title = title;
            row.project = project;
        }
    }
    *state.live_activity.current.lock().unwrap() = Some(aggregate.clone());
    #[cfg(test)]
    state
        .live_activity
        .milestones
        .lock()
        .unwrap()
        .push(("aggregation", std::time::Instant::now()));
    Some(aggregate)
}

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("live activity HTTP client configuration")
    })
}

enum Sent {
    Delivered(serde_json::Value),
    /// The Worker refused it (the registration is gone or the request invalid).
    Rejected,
    Retry,
}

/// `activity` reports this Mac's chats; `activity-state` forwards the phone's
/// word about its card.
async fn send(
    target: &wonder_store::LiveActivityTarget,
    action: &str,
    body: &serde_json::Value,
) -> Sent {
    let result = http()
        .post(format!(
            "{}/v1/registrations/{}/{action}",
            target.endpoint, target.registration_id
        ))
        .bearer_auth(&target.sender_secret)
        .json(body)
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            Sent::Delivered(response.json().await.unwrap_or_default())
        }
        Ok(response) if matches!(response.status().as_u16(), 400 | 403 | 410) => Sent::Rejected,
        _ => Sent::Retry,
    }
}

/// Report this Mac's rows to every registration that has not seen them.
pub(crate) async fn deliver(state: &AppState, aggregate: &Aggregate, now: i64) {
    report(state, &aggregate.rows, now).await;
}

async fn report(state: &AppState, rows: &[Row], now: i64) {
    let Ok(targets) = state.store.live_activity_registrations().await else {
        return;
    };
    // A policy can change after the observation but before transport.
    let mut shown = Vec::new();
    for row in rows {
        match state.store.chat_notification_level(&row.thread).await {
            Ok(Some(wonder_store::ChatNotificationLevel::Mute)) | Err(_) => {}
            _ => shown.push(row),
        }
    }
    shown.truncate(ROW_LIMIT);
    let working = shown.iter().any(|row| !row.status.is_finished());
    state
        .live_activity
        .reported
        .lock()
        .unwrap()
        .retain(|id, _| targets.iter().any(|target| &target.registration_id == id));
    for target in targets {
        let previous = state
            .live_activity
            .reported
            .lock()
            .unwrap()
            .get(&target.registration_id)
            .cloned();
        if previous
            .as_ref()
            .is_some_and(|seen| seen.next_attempt > now)
        {
            continue;
        }
        let mut body_rows = Vec::new();
        for row in &shown {
            // Reuses the device's route while the chat is the same.
            if let Ok(route) = state
                .store
                .live_activity_thread_route(&target.device_id, &row.thread)
                .await
            {
                body_rows.push(ReportRow {
                    route_id: route,
                    title: shorten(&row.title, TITLE_CHARS),
                    project: shorten(&row.project, PROJECT_CHARS),
                    status: row.status,
                    started_at: row.since,
                });
            }
        }
        let encoded = serde_json::to_string(&body_rows).unwrap_or_default();
        if let Some(seen) = &previous {
            let heartbeat = working && now - seen.at >= HEARTBEAT_SECONDS;
            let revisit = seen.revisit_at.is_some_and(|at| now >= at);
            if seen.encoded == encoded && !heartbeat && !revisit {
                continue;
            }
        }
        let entry = match send(
            &target,
            "activity",
            &serde_json::json!({ "rows": body_rows }),
        )
        .await
        {
            Sent::Delivered(reply) => {
                let stored = reply["stored"].as_bool().unwrap_or(true);
                let unstored = !stored && !body_rows.is_empty();
                Reported {
                    // A phone with no card yet stores nothing; ask again soon.
                    encoded: if unstored { String::new() } else { encoded },
                    at: now,
                    next_attempt: if unstored {
                        now + UNSTORED_RETRY_SECONDS
                    } else {
                        0
                    },
                    failures: 0,
                    revisit_at: reply["revisit"].as_i64().map(|seconds| now + seconds),
                }
            }
            Sent::Rejected | Sent::Retry => {
                let failures = previous.as_ref().map_or(0, |seen| seen.failures) + 1;
                Reported {
                    next_attempt: now + 300.min(5 << failures.min(6)),
                    failures,
                    ..previous.unwrap_or_default()
                }
            }
        };
        state
            .live_activity
            .reported
            .lock()
            .unwrap()
            .insert(target.registration_id.clone(), entry);
    }
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker: Option<Tracker> = None;
        let mut events = state.events.subscribe();
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = state.live_activity.wake.notified() => {
                    #[cfg(test)]
                    state.live_activity.milestones.lock().unwrap().push(("wake", std::time::Instant::now()));
                }
                received = events.recv() => {
                    if matches!(received, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return;
                    }
                    #[cfg(test)]
                    state.live_activity.milestones.lock().unwrap().push(("event", std::time::Instant::now()));
                    // Coalesce a burst of streaming events into one read.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    while events.try_recv().is_ok() {}
                }
            }
            match state.store.push_enabled().await {
                Ok(false) => {
                    tracker = None;
                    *state.live_activity.current.lock().unwrap() = None;
                    state.live_activity.reported.lock().unwrap().clear();
                    state.live_activity.desktop_ended.lock().unwrap().clear();
                    continue;
                }
                Err(_) => continue,
                Ok(true) => {}
            }
            if tracker.is_none() {
                tracker = Some(Tracker {
                    initial: Some(Phase::from_message_state(
                        state
                            .store
                            .newest_owned_outcome()
                            .await
                            .ok()
                            .flatten()
                            .map(|(_, outcome)| outcome)
                            .as_deref(),
                    )),
                    ..Tracker::default()
                });
            }
            let now = now_seconds();
            if let Some(aggregate) = evaluate(&state, tracker.as_mut().unwrap(), now).await {
                deliver(&state, &aggregate, now).await;
            }
        }
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Registration {
    /// `update` (a running activity's token), `start` (push-to-start), `ended`
    /// (that activity ended on the phone: forget its token), `dismissed` (the
    /// owner swiped the card away) or `foreground` (the owner opened Cupola
    /// again). An empty `update` or `start` token turns Live Activities off.
    kind: String,
    /// Hex APNs token; empty for `dismissed`, `foreground` and turning off.
    token: String,
    #[serde(flatten)]
    action: SignedActionFields,
}

/// The phone's word about its one card, signed over its pairing and forwarded to
/// the Worker, which owns the tokens and the dismissal gate. Replies with the
/// combined state when a token registers, so the phone paints it at once.
pub(crate) async fn register(
    State(state): State<AppState>,
    Extension(device): Extension<AuthenticatedDevice>,
    Json(request): Json<Registration>,
) -> Response {
    let state_only = matches!(request.kind.as_str(), "dismissed" | "foreground");
    let valid_token = if state_only {
        request.token.is_empty()
    } else {
        request.token.is_empty()
            || (request.token.len() >= 32
                && request.token.len() <= 512
                && request.token.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    if !matches!(
        request.kind.as_str(),
        "update" | "start" | "ended" | "dismissed" | "foreground"
    ) || !valid_token
        || (request.kind == "ended" && request.token.is_empty())
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(error) = verify_signed_action(
        &state,
        &device,
        "push.liveActivity",
        "/api/v1/push/live-activity",
        &action_body_sha256(&serde_json::json!([request.kind, request.token])),
        &request.action,
        "active",
    )
    .await
    {
        return (StatusCode::UNAUTHORIZED, error).into_response();
    }
    let target = match state.store.live_activity_registrations().await {
        Ok(targets) => targets
            .into_iter()
            .find(|target| target.device_id == device.device_id),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(target) = target else {
        // Live Activities follow the Mac only with notifications on.
        return (
            StatusCode::CONFLICT,
            "Turn on notifications for this computer to keep Live Activities current.",
        )
            .into_response();
    };
    let forwarded = match (request.kind.as_str(), request.token.is_empty()) {
        ("update" | "start", true) => serde_json::json!({"kind": "off"}),
        (kind, true) => serde_json::json!({ "kind": kind }),
        (kind, false) => serde_json::json!({"kind": kind, "token": request.token}),
    };
    match send(&target, "activity-state", &forwarded).await {
        Sent::Delivered(reply) => {
            // A new card stores rows now: report this Mac's chats again at once.
            if matches!(request.kind.as_str(), "update" | "start") {
                state
                    .live_activity
                    .reported
                    .lock()
                    .unwrap()
                    .remove(&target.registration_id);
                state.live_activity.wake.notify_one();
            }
            let mut body = serde_json::json!({
                "registered": !state_only && reply["registered"].as_bool().unwrap_or(false),
            });
            if reply["dismissed"].as_bool() == Some(true) && request.kind == "update" {
                body["dismissed"] = true.into();
            }
            if !reply["state"].is_null() {
                body["state"] = reply["state"].clone();
            }
            Json(body).into_response()
        }
        Sent::Rejected | Sent::Retry => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(all(test, unix))]
#[path = "live_activity_tests.rs"]
mod tests;
