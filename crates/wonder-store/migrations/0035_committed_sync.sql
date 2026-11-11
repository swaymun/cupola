-- The journal records invalidations in the transaction that changes a projection.
-- Presentation events are optional hints; snapshots remain the state authority.
CREATE TABLE sync_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    host_epoch TEXT NOT NULL,
    start_sequence INTEGER NOT NULL,
    last_sequence INTEGER NOT NULL
);
CREATE TABLE sync_journal (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    host_epoch TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    conversation_id TEXT,
    resource TEXT,
    change_kind TEXT,
    payload_json TEXT
);
CREATE TRIGGER sync_journal_committed_head AFTER INSERT ON sync_journal BEGIN
    UPDATE sync_state SET last_sequence = NEW.sequence WHERE singleton = 1;
    DELETE FROM sync_journal WHERE sequence <= NEW.sequence - 1024;
END;

CREATE TRIGGER sync_messages_insert AFTER INSERT ON messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'messages', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_messages_update AFTER UPDATE ON messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'messages', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_messages_delete AFTER DELETE ON messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.conversation_id, 'messages', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_assistant_messages_insert AFTER INSERT ON assistant_messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'assistant_messages', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_assistant_messages_update AFTER UPDATE ON assistant_messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'assistant_messages', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_assistant_messages_delete AFTER DELETE ON assistant_messages BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.conversation_id, 'assistant_messages', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_message_attachments_insert AFTER INSERT ON message_attachments BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), (SELECT conversation_id FROM messages WHERE id = NEW.message_id), 'message_attachments', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_message_attachments_update AFTER UPDATE ON message_attachments BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), (SELECT conversation_id FROM messages WHERE id = NEW.message_id), 'message_attachments', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_message_attachments_delete AFTER DELETE ON message_attachments BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), (SELECT conversation_id FROM messages WHERE id = OLD.message_id), 'message_attachments', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_conversations_insert AFTER INSERT ON conversations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.id, 'conversations', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_conversations_update AFTER UPDATE ON conversations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.id, 'conversations', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_conversations_delete AFTER DELETE ON conversations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.id, 'conversations', 'delete'
    FROM sync_state WHERE singleton = 1;
END;







CREATE TRIGGER sync_conversation_files_insert AFTER INSERT ON conversation_files BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'conversation_files', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_conversation_files_update AFTER UPDATE ON conversation_files BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'conversation_files', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_conversation_files_delete AFTER DELETE ON conversation_files BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.conversation_id, 'conversation_files', 'delete'
    FROM sync_state WHERE singleton = 1;
END;







CREATE TRIGGER sync_approvals_insert AFTER INSERT ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'approvals', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_approvals_update AFTER UPDATE ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'approvals', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_approvals_delete AFTER DELETE ON approvals BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'approvals', 'delete'
    FROM sync_state WHERE singleton = 1;
END;










CREATE TRIGGER sync_automations_insert AFTER INSERT ON automations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'automations', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automations_update AFTER UPDATE ON automations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.conversation_id, 'automations', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automations_delete AFTER DELETE ON automations BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.conversation_id, 'automations', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automation_runs_insert AFTER INSERT ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'automation_runs', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automation_runs_update AFTER UPDATE ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'automation_runs', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_automation_runs_delete AFTER DELETE ON automation_runs BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'automation_runs', 'delete'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_devices_insert AFTER INSERT ON devices BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'devices', 'insert'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_devices_update AFTER UPDATE ON devices BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'devices', 'update'
    FROM sync_state WHERE singleton = 1;
END;

CREATE TRIGGER sync_devices_delete AFTER DELETE ON devices BEGIN
    INSERT INTO sync_journal(host_epoch, occurred_at, conversation_id, resource, change_kind)
    SELECT host_epoch, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, 'devices', 'delete'
    FROM sync_state WHERE singleton = 1;
END;
