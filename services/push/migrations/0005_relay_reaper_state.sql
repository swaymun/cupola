-- Idle deletion counts from the first cron that observed a tunnel offline,
-- never from the last time it was seen online (which ran up to 5 minutes early).
ALTER TABLE relay_hosts ADD COLUMN offline_since INTEGER;
-- Tombstones form the purge queue: due rows first, retried with backoff so a
-- permanently failing purge cannot starve the others.
ALTER TABLE relay_hosts ADD COLUMN purge_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE relay_hosts ADD COLUMN purge_after INTEGER NOT NULL DEFAULT 0;
-- One shared capacity snapshot, refreshed by cron, so anonymous health checks
-- and signed-in capacity reads never fan out to the Cloudflare API.
CREATE TABLE relay_capacity (id TEXT PRIMARY KEY CHECK(id='current'), value TEXT NOT NULL, updated INTEGER NOT NULL) WITHOUT ROWID;
