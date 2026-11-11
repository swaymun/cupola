CREATE TABLE IF NOT EXISTS automation_runs (
    id TEXT PRIMARY KEY NOT NULL,
    automation_id TEXT NOT NULL REFERENCES automations(id) ON DELETE CASCADE,
    scheduled_for TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    started_at TEXT NOT NULL,
    finished_at TEXT,
    error TEXT,
    message_id TEXT,
    UNIQUE (automation_id, scheduled_for)
);

CREATE INDEX IF NOT EXISTS automation_runs_automation_idx ON automation_runs(automation_id, started_at DESC);
