-- Voice limits and transcripts (owner, Oct 21). One row of owner settings;
-- one row per voice panel the phone opened, with the usage it reported, so
-- the Mac can refuse new sessions past the daily budget. Transcripts are
-- text only (no audio), kept apart from usage so deleting them leaves the
-- day's budget intact. Rows are removed explicitly; the store does not
-- enable foreign keys.
CREATE TABLE voice_settings (
  id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
  session_limit_minutes INTEGER NOT NULL CHECK (session_limit_minutes BETWEEN 1 AND 55),
  daily_minutes INTEGER CHECK (daily_minutes IS NULL OR daily_minutes BETWEEN 1 AND 1440),
  daily_cents INTEGER CHECK (daily_cents IS NULL OR daily_cents BETWEEN 1 AND 100000),
  keep_transcripts INTEGER NOT NULL CHECK (keep_transcripts IN (0, 1)),
  updated_at TEXT NOT NULL
);

CREATE TABLE voice_panels (
  id TEXT PRIMARY KEY NOT NULL,
  device_id TEXT NOT NULL,
  opened_at_ms INTEGER NOT NULL,
  -- Fixed when the panel opens: reconnecting does not extend a session.
  ends_at_ms INTEGER NOT NULL,
  ended_at_ms INTEGER,
  mints INTEGER NOT NULL CHECK (mints >= 0),
  connected_seconds INTEGER NOT NULL DEFAULT 0 CHECK (connected_seconds >= 0)
);
CREATE INDEX voice_panels_opened ON voice_panels(opened_at_ms);

-- One row per OpenAI usage event, so a repeated or late report counts once.
CREATE TABLE voice_usage_events (
  panel_id TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('response', 'transcription')),
  event_id TEXT NOT NULL,
  cost_microdollars INTEGER NOT NULL CHECK (cost_microdollars >= 0),
  PRIMARY KEY (panel_id, source, event_id)
);

CREATE TABLE voice_transcript_entries (
  panel_id TEXT NOT NULL,
  item_id TEXT NOT NULL,
  speaker TEXT NOT NULL CHECK (speaker IN ('owner', 'voice')),
  text TEXT NOT NULL,
  at_ms INTEGER NOT NULL,
  PRIMARY KEY (panel_id, item_id)
);

-- The threads a voice session acted on (sent to, started, stopped, answered).
CREATE TABLE voice_transcript_threads (
  panel_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  PRIMARY KEY (panel_id, thread_id)
);
CREATE INDEX voice_transcript_threads_thread ON voice_transcript_threads(thread_id);
