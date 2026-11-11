-- A Project's Outputs index reads the conversation files its threads'
-- agents produced; nothing here copies or crawls files.
-- `replaces_file_id`: the earlier output this one updates (a visual called
-- again with `replace`), so the index can show the newer revision.
ALTER TABLE conversation_files ADD COLUMN replaces_file_id TEXT;

-- Outputs the owner pinned to the top of the index. A pin goes with its file.
CREATE TABLE conversation_file_pins (
    file_id TEXT PRIMARY KEY NOT NULL
        REFERENCES conversation_files(id) ON DELETE CASCADE,
    pinned_at TEXT NOT NULL
);

-- 0137 compares every column before journaling an update; include the new one.
DROP TRIGGER sync_conversation_files_update;
CREATE TRIGGER sync_conversation_files_update AFTER UPDATE ON conversation_files
WHEN OLD.id IS NOT NEW.id
  OR OLD.conversation_id IS NOT NEW.conversation_id
  OR OLD.kind IS NOT NEW.kind
  OR OLD.name IS NOT NEW.name
  OR OLD.relative_path IS NOT NEW.relative_path
  OR OLD.state IS NOT NEW.state
  OR OLD.additions IS NOT NEW.additions
  OR OLD.deletions IS NOT NEW.deletions
  OR OLD.source_id IS NOT NEW.source_id
  OR OLD.created_at IS NOT NEW.created_at
  OR OLD.updated_at IS NOT NEW.updated_at
  OR OLD.mime_type IS NOT NEW.mime_type
  OR OLD.byte_size IS NOT NEW.byte_size
  OR OLD.sha256 IS NOT NEW.sha256
  OR OLD.replaces_file_id IS NOT NEW.replaces_file_id
BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'conversation_files', 'update'
    FROM sync_state WHERE singleton = 1;
END;
