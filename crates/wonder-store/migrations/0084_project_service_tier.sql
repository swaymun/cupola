-- A Project thread keeps its selected speed across reconnects and reloads.
ALTER TABLE project_conversations ADD COLUMN service_tier TEXT;

-- Older queued messages have no historical speed snapshot. Preserve the
-- settings visible at upgrade time; new sends snapshot at acceptance.
INSERT OR IGNORE INTO message_execution_settings
 (message_id,model,reasoning_effort,service_tier,working_directory)
SELECT m.id,p.model,p.effort,p.service_tier,p.cwd
FROM messages m JOIN dispatch_work w ON w.message_id=m.id
JOIN project_conversations p ON p.conversation_id=m.conversation_id
WHERE m.state='accepted_by_wonder';
