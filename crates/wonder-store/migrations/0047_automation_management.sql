-- Snapshot the accepted task so edits affect future runs only. Existing runs
-- remain governed by their durable message/dispatch state.
ALTER TABLE automation_runs ADD COLUMN automation_snapshot TEXT;
ALTER TABLE automation_runs ADD COLUMN conversation_id TEXT;
UPDATE automation_runs SET conversation_id = (SELECT conversation_id FROM messages WHERE messages.id = automation_runs.message_id);
CREATE INDEX automation_runs_active_idx ON automation_runs(automation_id, status);
