-- Approvals and automation runs journal the conversation that owns them, so a
-- phone refreshes that chat (and the list) instead of every cached chat. A row
-- whose owner could not be established still journals NULL, which clients treat
-- as a host-wide change.
ALTER TABLE approvals ADD COLUMN conversation_id TEXT;

DROP TRIGGER sync_approvals_insert;
DROP TRIGGER sync_approvals_update;
DROP TRIGGER sync_approvals_delete;
DROP TRIGGER sync_automation_runs_insert;
DROP TRIGGER sync_automation_runs_update;
DROP TRIGGER sync_automation_runs_delete;

CREATE TRIGGER sync_approvals_insert AFTER INSERT ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'approvals', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_approvals_update AFTER UPDATE ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'approvals', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_approvals_delete AFTER DELETE ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.conversation_id, 'approvals', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

-- A run names its conversation once its message is sent (0047); before that,
-- the automation's own conversation is the one it runs in, when it has one.
CREATE TRIGGER sync_automation_runs_insert AFTER INSERT ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
        COALESCE(NEW.conversation_id, (SELECT conversation_id FROM messages WHERE id = NEW.message_id), (SELECT conversation_id FROM automations WHERE id = NEW.automation_id)),
        'automation_runs', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automation_runs_update AFTER UPDATE ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
        COALESCE(NEW.conversation_id, (SELECT conversation_id FROM messages WHERE id = NEW.message_id), (SELECT conversation_id FROM automations WHERE id = NEW.automation_id)),
        'automation_runs', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automation_runs_delete AFTER DELETE ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
        COALESCE(OLD.conversation_id, (SELECT conversation_id FROM messages WHERE id = OLD.message_id), (SELECT conversation_id FROM automations WHERE id = OLD.automation_id)),
        'automation_runs', 'delete'
    FROM sync_state WHERE singleton = 1;
END;
