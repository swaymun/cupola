-- Replaying an unchanged agent picture rewrites its conversation_files row
-- with the same values. That must not journal an update: each entry wakes
-- connected phones and shortens how far back sync can replay. Every column
-- is compared, so any real change (including conversation_id and
-- source_id) still journals; a column added later must be added here.
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
BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'conversation_files', 'update'
    FROM sync_state WHERE singleton = 1;
END;
