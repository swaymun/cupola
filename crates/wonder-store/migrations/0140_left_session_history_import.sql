-- A session a thread left may hold history Wonder never saved (turns made on
-- the Mac). Until that history is imported, this holds the timeline position
-- (milliseconds) the imported items must stay before: the switch row. NULL
-- means nothing is owed. Kept with the left session so a busy, slow or
-- interrupted import is retried after a restart instead of being dropped.
ALTER TABLE project_runtime_history ADD COLUMN history_import_before_ms INTEGER;
