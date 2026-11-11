-- One Live Activity per phone, rebuilt here from every computer's chats.
-- A row is one chat on one computer (registration): a random route ID, a short
-- title and Project name, a status and two times. Rows are short-lived: running
-- rows expire after 2 hours, waiting rows after 24 hours and finished rows after
-- 15 minutes (the reaper deletes what the aggregate already ignores). No chat
-- text, conversation IDs or provider names are stored.
CREATE TABLE activity_rows (
 registration_id TEXT NOT NULL,
 route_id TEXT NOT NULL,
 session_id TEXT NOT NULL,
 title TEXT NOT NULL,
 project TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('running','needsApproval','needsInput','done','failed')),
 started INTEGER NOT NULL,
 updated INTEGER NOT NULL,
 PRIMARY KEY(registration_id,route_id)
) WITHOUT ROWID;
CREATE INDEX activity_rows_session ON activity_rows(session_id);
CREATE INDEX activity_rows_updated ON activity_rows(updated);
-- The phone's card: its ActivityKit tokens, whether the owner swiped it away,
-- and the last state pushed (a hash, never content). `dismissed` stops every
-- update and push-to-start until the owner opens Cupola again.
CREATE TABLE activity_cards (
 session_id TEXT PRIMARY KEY,
 update_token TEXT,
 start_token TEXT,
 dismissed INTEGER NOT NULL DEFAULT 0,
 sent_hash TEXT,
 sent_at INTEGER NOT NULL DEFAULT 0,
 start_sent_at INTEGER NOT NULL DEFAULT 0,
 work_since INTEGER NOT NULL DEFAULT 0,
 updated INTEGER NOT NULL
) WITHOUT ROWID;
