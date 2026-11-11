-- Pull requests opened from Wonder's Code changes sheet, linked to the chat
-- that opened them so its repository index lists them as "Opened here".
-- The repository path stays on the Mac; the phone never receives it.
CREATE TABLE thread_pull_request_links (
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  url TEXT NOT NULL,
  repository_path TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (conversation_id, url)
);
