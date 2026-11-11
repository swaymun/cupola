-- Only a completed read of a stable provider source can reuse this checkpoint.
-- Reset/rewind already invalidate file_change_version and the refresh token.
ALTER TABLE history_hydration ADD COLUMN source_thread TEXT;
ALTER TABLE history_hydration ADD COLUMN source_revision TEXT;
