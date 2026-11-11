-- The owner's Stop on a running turn, kept so a host restart before the
-- provider's terminal status still reads that turn as stopped, never lost.
CREATE TABLE turn_stop_requests (
 thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, requested_at INTEGER NOT NULL,
 PRIMARY KEY(thread_id, turn_id)
);
