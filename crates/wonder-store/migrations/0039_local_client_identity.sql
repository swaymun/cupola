-- Local process identity is an attribution record, not a remotely paired key.
ALTER TABLE devices ADD COLUMN is_local INTEGER NOT NULL DEFAULT 0 CHECK (is_local IN (0, 1));
