-- ActivityKit push tokens a paired phone registered for its Live Activities.
-- `update` tokens address one running activity; a `start` token lets the Mac
-- start one (push-to-start). Only the last content sent is kept, never text.
CREATE TABLE live_activity_tokens (
 token TEXT PRIMARY KEY,
 device_id TEXT NOT NULL REFERENCES devices(id),
 kind TEXT NOT NULL CHECK(kind IN ('update','start')),
 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
 sent_state TEXT,
 sent_at INTEGER,
 route_id TEXT,
 failures INTEGER NOT NULL DEFAULT 0,
 next_attempt INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX live_activity_tokens_device ON live_activity_tokens(device_id,kind);
