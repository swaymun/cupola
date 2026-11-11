//! Claude Code sessions archived in the Claude app (owner, Oct 23: threads
//! archived in their home app leave Cupola's Chats too). The Claude app
//! keeps a record per Claude Code session it runs,
//! `claude-code-sessions/<account>/<organization>/local_<id>.json`, with the
//! Claude Code session it continues (`cliSessionId`) and `isArchived`, and an
//! index of the archived ones beside them (`archived-sessions.idx`,
//! `{"v":1,"archived":["local_<id>", …]}`; read on Oct 23, it listed exactly
//! the records marked archived). Only these files are read, never written:
//! the Claude app offers no supported way for another app to unarchive, so a
//! restore in Cupola shows the thread in Cupola only.
//!
//! The indexes are looked at most every `FRESH` and read again only once one
//! changed; a record is read once per change to it (they run to megabytes).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const FRESH: Duration = Duration::from_secs(5);
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FOLDERS: usize = 64;
const MAX_ARCHIVED: usize = 20_000;

type Stamp = Option<(SystemTime, u64)>;

/// A Claude Code session the Claude app archived, and when its record last
/// changed (seconds since the epoch): it was archived then or before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Archived {
    pub(crate) record: String,
    pub(crate) since: i64,
}

#[derive(Default)]
pub struct ClaudeApp {
    /// `~/Library/Application Support/Claude/claude-code-sessions`; none in
    /// tests unless one gives its own.
    root: Option<PathBuf>,
    read: tokio::sync::Mutex<Read>,
}

#[derive(Default)]
struct Read {
    at: Option<Instant>,
    indexes: Vec<(PathBuf, Stamp)>,
    /// Each record read: its stamp, the session it continues and whether it
    /// says archived.
    records: HashMap<PathBuf, (Stamp, Option<(String, bool)>)>,
    archived: HashMap<String, Archived>,
}

impl ClaudeApp {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            read: Default::default(),
        }
    }

    /// The Claude app's own folder for this user.
    pub fn on_this_mac() -> Self {
        Self::new(std::env::var_os("HOME").map(|home| {
            PathBuf::from(home).join("Library/Application Support/Claude/claude-code-sessions")
        }))
    }

    /// How the Claude app archived this Claude Code session, if it did.
    pub(crate) async fn archived(&self, session: &str) -> Option<Archived> {
        let root = self.root.clone()?;
        let mut read = self.read.lock().await;
        if !read.at.is_some_and(|at| at.elapsed() < FRESH) {
            let previous = std::mem::take(&mut *read);
            let mut next = tokio::task::spawn_blocking(move || refreshed(&root, previous))
                .await
                .unwrap_or_default();
            next.at = Some(Instant::now());
            *read = next;
        }
        read.archived.get(session).cloned()
    }
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The index files under `root/<account>/<organization>/`.
fn indexes(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(accounts) = std::fs::read_dir(root) else {
        return found;
    };
    for account in accounts.flatten().take(MAX_FOLDERS) {
        let Ok(organizations) = std::fs::read_dir(account.path()) else {
            continue;
        };
        for organization in organizations.flatten().take(MAX_FOLDERS) {
            let index = organization.path().join("archived-sessions.idx");
            if index.is_file() {
                found.push(index);
            }
        }
    }
    found.sort();
    found
}

fn refreshed(root: &Path, mut read: Read) -> Read {
    let indexes: Vec<_> = indexes(root)
        .into_iter()
        .map(|path| {
            let stamp = stamp(&path);
            (path, stamp)
        })
        .collect();
    if indexes == read.indexes {
        return read;
    }
    let mut archived = HashMap::new();
    let mut records = HashMap::new();
    for (index, index_stamp) in &indexes {
        if index_stamp.is_none_or(|(_, len)| len > MAX_INDEX_BYTES) {
            continue;
        }
        let Some(listed) = std::fs::read(index)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            continue;
        };
        let folder = index.parent().unwrap_or(root);
        for local in listed["archived"].as_array().into_iter().flatten() {
            let Some(local) = local.as_str().filter(|local| valid_record(local)) else {
                continue;
            };
            if archived.len() >= MAX_ARCHIVED {
                break;
            }
            let path = folder.join(format!("{local}.json"));
            let now = stamp(&path);
            let known = read
                .records
                .remove(&path)
                .filter(|(was, _)| *was == now && now.is_some());
            let (record_stamp, session) = match known {
                Some(known) => known,
                None => (now, now.and_then(|(_, len)| read_record(&path, len))),
            };
            // Archived only when its own record still says so.
            if let (Some((modified, _)), Some((session, true))) = (record_stamp, &session) {
                let since = modified
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs() as i64);
                archived.insert(
                    session.clone(),
                    Archived {
                        record: local.to_owned(),
                        since,
                    },
                );
            }
            records.insert(path, (record_stamp, session));
        }
    }
    Read {
        at: read.at,
        indexes,
        records,
        archived,
    }
}

fn valid_record(local: &str) -> bool {
    local.starts_with("local_")
        && local.len() <= 80
        && local[6..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The Claude Code session a record continues, and whether it says archived.
fn read_record(path: &Path, len: u64) -> Option<(String, bool)> {
    if len > MAX_RECORD_BYTES {
        return None;
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Record {
        cli_session_id: Option<String>,
        #[serde(default)]
        is_archived: bool,
    }
    let record: Record = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some((record.cli_session_id?, record.is_archived))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: serde_json::Value) {
        std::fs::write(dir.join(name), body.to_string()).unwrap();
    }

    // Contract (owner, Oct 23): a Claude Code session the Claude app
    // archived is known as archived; one it shows, or one whose record no
    // longer says archived, is not. Unarchiving there shows it again.
    #[tokio::test]
    async fn sessions_the_claude_app_archived_are_known() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("account/organization");
        std::fs::create_dir_all(&folder).unwrap();
        write(
            &folder,
            "local_a.json",
            serde_json::json!({"cliSessionId": "session-a", "isArchived": true, "title": "A"}),
        );
        write(
            &folder,
            "local_b.json",
            serde_json::json!({"cliSessionId": "session-b", "isArchived": false}),
        );
        write(
            &folder,
            "local_c.json",
            serde_json::json!({"cliSessionId": "session-c", "isArchived": false}),
        );
        write(
            &folder,
            "archived-sessions.idx",
            serde_json::json!({"v": 1, "archived": ["local_a", "local_c", "../x"]}),
        );
        let app = ClaudeApp::new(Some(root.path().to_owned()));
        assert_eq!(
            app.archived("session-a").await.map(|a| a.record),
            Some("local_a".to_owned())
        );
        assert_eq!(app.archived("session-b").await, None);
        assert_eq!(
            app.archived("session-c").await,
            None,
            "its record disagrees"
        );

        // Unarchived in the Claude app.
        write(
            &folder,
            "local_a.json",
            serde_json::json!({"cliSessionId": "session-a", "isArchived": false, "title": "A again"}),
        );
        write(
            &folder,
            "archived-sessions.idx",
            serde_json::json!({"v": 1, "archived": []}),
        );
        assert!(
            app.archived("session-a").await.is_some(),
            "read again after a while"
        );
        app.read.lock().await.at = None;
        assert_eq!(app.archived("session-a").await, None);

        assert_eq!(ClaudeApp::default().archived("session-a").await, None);
    }
}
