CREATE TABLE IF NOT EXISTS transcriptions (
    id TEXT PRIMARY KEY NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'processing', 'completed', 'failed', 'cancelled')),
    source_device_id TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    transcript_text TEXT,
    word_timestamps_json TEXT,
    confidence REAL,
    retry_expires_at_ms INTEGER,
    error_category TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
