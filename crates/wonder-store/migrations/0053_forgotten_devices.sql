-- Keep revoked identity references needed by saved conversation history.
ALTER TABLE devices ADD COLUMN forgotten INTEGER NOT NULL DEFAULT 0 CHECK (forgotten IN (0, 1) AND (forgotten = 0 OR (revoked_at IS NOT NULL AND is_local = 0)));
