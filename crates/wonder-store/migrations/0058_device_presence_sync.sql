-- Request authentication updates last_seen_at. It must not invalidate chats:
-- a refresh would itself authenticate and create another refresh indefinitely.
DROP TRIGGER sync_devices_update;
CREATE TRIGGER sync_devices_update AFTER UPDATE ON devices
WHEN OLD.label IS NOT NEW.label
  OR OLD.role IS NOT NEW.role
  OR OLD.public_key_jwk IS NOT NEW.public_key_jwk
  OR OLD.revoked_at IS NOT NEW.revoked_at
  OR OLD.session_expires_at_ms IS NOT NEW.session_expires_at_ms
  OR OLD.is_local IS NOT NEW.is_local
  OR OLD.forgotten IS NOT NEW.forgotten
BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'devices', 'update'
    FROM sync_state WHERE singleton = 1;
END;
