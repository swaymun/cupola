//! Threads closed in their home app (owner, Oct 23): a Claude Code session
//! or Codex thread that T3 Code settled, snoozed, archived or deleted, or a
//! Claude Code session the Claude app archived, leaves Cupola's Chats like a
//! thread archived in Cupola and is listed in the Project's Archived threads,
//! saying where. Codex's own archive is read elsewhere (`projects`).
//!
//! It shows in Chats again once the home app reopens it, and stays shown
//! while:
//! - it is pinned in Cupola (as pinning in T3 Code unsettles a thread),
//! - its agent is working,
//! - it was used in Cupola after it was closed (Cupola's own activity: a
//!   transcript's file time also changes without a message), or
//! - the owner restored it in Cupola from that closing (`restore`).
//!
//! Restore reopens a T3 thread in T3 Code through T3 Code's own tool when
//! Cupola is connected to T3 Code; otherwise, and for the Claude app (which
//! has no supported way for another app to unarchive) or a T3 thread T3 Code
//! deleted, it shows the thread in Cupola only, until the home app closes it
//! anew.

use std::collections::HashMap;

use wonder_store::{AgentFamily, StoredProjectConversation};

use crate::t3_code::titles::{Closed, Closing};
use crate::AppState;

/// Where and how a thread was closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Elsewhere {
    T3Code(Closing),
    /// The Claude app's record of the session (`local_<id>`).
    ClaudeApp(crate::claude_app::Archived),
}

impl Elsewhere {
    /// Names this closing, so a restore in Cupola lasts until the home app
    /// closes the thread anew.
    pub(crate) fn marker(&self) -> String {
        match self {
            Self::T3Code(closing) => {
                let how = match closing.how {
                    Closed::Settled => "settled",
                    Closed::Snoozed(_) => "snoozed",
                    Closed::Archived => "archived",
                    Closed::Deleted => "deleted",
                };
                format!("t3:{}:{how}:{}", closing.thread, closing.since)
            }
            Self::ClaudeApp(archived) => format!("claude-app:{}:archived", archived.record),
        }
    }

    /// What the Archived list says of it.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::T3Code(closing) => match closing.how {
                Closed::Settled => "Settled in T3 Code".into(),
                Closed::Snoozed(_) => "Snoozed in T3 Code".into(),
                Closed::Archived => "Archived in T3 Code".into(),
                Closed::Deleted => "Deleted in T3 Code".into(),
            },
            Self::ClaudeApp(_) => "Archived in the Claude app".into(),
        }
    }

    /// When a snoozed thread returns (RFC 3339).
    pub(crate) fn until(&self) -> Option<String> {
        match self {
            Self::T3Code(Closing {
                how: Closed::Snoozed(until),
                ..
            }) => chrono::DateTime::from_timestamp_millis(*until)
                .map(|until| until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            _ => None,
        }
    }

    /// Whether Restore reopens it in its home app too (when Cupola is
    /// connected there).
    pub(crate) fn reopens_there(&self) -> bool {
        matches!(self, Self::T3Code(closing) if closing.how != Closed::Deleted)
    }

    /// When it was closed, in seconds since the epoch.
    fn since(&self) -> i64 {
        match self {
            Self::T3Code(closing) => chrono::DateTime::parse_from_rfc3339(&closing.since)
                .map(|at| at.timestamp())
                .unwrap_or(i64::MAX),
            Self::ClaudeApp(archived) => archived.since,
        }
    }
}

/// How the thread's home app closed it: only the Mac's own Claude Code and
/// Codex homes, which T3 Code and the Claude app use.
pub(crate) async fn closing(
    state: &AppState,
    family: AgentFamily,
    store: &str,
    native: &str,
) -> Option<Elsewhere> {
    if !crate::providers::is_default_store(state, family, store) {
        return None;
    }
    if let Some(closing) = state.t3_code.closing(family, native).await {
        return Some(Elsewhere::T3Code(closing));
    }
    if family == AgentFamily::Claude {
        return state
            .claude_app
            .archived(native)
            .await
            .map(Elsewhere::ClaudeApp);
    }
    None
}

/// Longest Restore waits for T3 Code to reopen a thread before showing it in
/// Cupola only.
const REOPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Restores in Cupola, read once per list (`kept_open_elsewhere`).
pub(crate) type Kept = HashMap<String, String>;

pub(crate) async fn kept(state: &AppState) -> Kept {
    state.store.kept_open_elsewhere().await.unwrap_or_default()
}

/// The closing that keeps this thread out of Chats, if one does. `owner`
/// is the Cupola thread the session belongs to, when there is one.
pub(crate) async fn hidden(
    state: &AppState,
    family: AgentFamily,
    store: &str,
    native: &str,
    owner: Option<&StoredProjectConversation>,
    working: bool,
    kept: &Kept,
) -> Option<Elsewhere> {
    if working || owner.is_some_and(|owner| owner.is_pinned) {
        return None;
    }
    let closing = closing(state, family, store, native).await?;
    if let Some(owner) = owner {
        if kept.get(&owner.conversation_id) == Some(&closing.marker()) {
            return None;
        }
        let used = chrono::DateTime::parse_from_rfc3339(&owner.last_activity_at)
            .map(|at| at.timestamp())
            .unwrap_or_default();
        if used > closing.since() {
            return None;
        }
    }
    Some(closing)
}

/// Restore of a thread closed in its home app: reopened there when that app
/// can (T3 Code, connected), and shown in Cupola either way. Whether the
/// home app reopened it.
pub(crate) async fn restore(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<bool, String> {
    let Some(native) = conversation.native_session_id.as_deref() else {
        return Ok(false);
    };
    let Some(closing) = closing(
        state,
        conversation.family,
        &conversation.provider_store,
        native,
    )
    .await
    else {
        return Ok(false);
    };
    let reopened = match &closing {
        Elsewhere::T3Code(t3) => tokio::time::timeout(REOPEN_TIMEOUT, state.t3_code.reopen(t3))
            .await
            .is_ok_and(|reopened| reopened.is_ok()),
        Elsewhere::ClaudeApp(_) => false,
    };
    // Kept in Cupola too: T3 Code may file the change a moment later.
    state
        .store
        .keep_open_elsewhere(&conversation.conversation_id, &closing.marker())
        .await
        .map_err(|_| "The restore could not be saved. Try again.".to_owned())?;
    Ok(reopened)
}
