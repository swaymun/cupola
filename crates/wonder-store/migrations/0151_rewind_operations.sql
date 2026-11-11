-- Commit the intent before contacting the provider. Only settlement against
-- the same operation may release its durable send/history fence.
CREATE TABLE rewind_operations (
    conversation_id TEXT PRIMARY KEY REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL UNIQUE,
    intent_json TEXT NOT NULL CHECK(json_valid(intent_json)),
    response_received INTEGER NOT NULL DEFAULT 0 CHECK(response_received IN (0,1)),
    -- The daemon's own provider call ended without a usable reply (a lost
    -- transport or timeout). Recovery inspects the provider after a short
    -- grace, since a timed-out call may still apply late.
    call_ended_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TRIGGER rewind_blocks_new_messages BEFORE INSERT ON messages
WHEN EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'Rewind recovery is pending. Try again shortly.'); END;

CREATE TRIGGER rewind_blocks_dispatch BEFORE UPDATE OF state ON messages
WHEN NEW.state IN ('dispatching_to_codex','accepted_by_codex','streaming')
 AND OLD.state IS NOT NEW.state
 AND EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'Rewind recovery is pending. Try again shortly.'); END;

CREATE TRIGGER rewind_preserves_binding BEFORE UPDATE ON runtime_bindings
WHEN (OLD.runtime_thread_id IS NOT NEW.runtime_thread_id OR OLD.session_id IS NOT NEW.session_id
 OR OLD.agent_family IS NOT NEW.agent_family OR OLD.provider_store IS NOT NEW.provider_store)
 AND EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=OLD.conversation_id)
BEGIN SELECT RAISE(ABORT,'Rewind recovery is pending. Try again shortly.'); END;

CREATE TRIGGER rewind_preserves_native_session BEFORE UPDATE ON project_conversations
WHEN (OLD.native_session_id IS NOT NEW.native_session_id OR OLD.agent_family IS NOT NEW.agent_family
 OR OLD.provider_store IS NOT NEW.provider_store)
 AND EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=OLD.conversation_id)
BEGIN SELECT RAISE(ABORT,'Rewind recovery is pending. Try again shortly.'); END;

-- Deleting the conversation itself cascades through its binding and releases
-- the fence; only unbinding a conversation that stays is refused.
CREATE TRIGGER rewind_preserves_binding_delete BEFORE DELETE ON runtime_bindings
WHEN EXISTS(SELECT 1 FROM rewind_operations WHERE conversation_id=OLD.conversation_id)
 AND EXISTS(SELECT 1 FROM conversations WHERE id=OLD.conversation_id)
BEGIN SELECT RAISE(ABORT,'Rewind recovery is pending. Try again shortly.'); END;

-- A removed turn may not have been indexed yet. Keep its identity so a late
-- provider notification cannot introduce it for the first time after recovery.
CREATE TABLE rewound_turns (
    conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    PRIMARY KEY(conversation_id,thread_id,turn_id)
);
CREATE TRIGGER rewind_hides_late_history AFTER INSERT ON history_entries
WHEN NEW.hidden_by_reset=0
BEGIN
 UPDATE history_entries SET hidden_by_reset=1 WHERE sequence=NEW.sequence AND EXISTS(
  SELECT 1 FROM rewound_turns r WHERE r.conversation_id=NEW.conversation_id AND (
   (NEW.source='event' AND r.thread_id=json_extract(NEW.payload_json,'$.threadId') AND r.turn_id=json_extract(NEW.payload_json,'$.turnId'))
   OR (NEW.source='assistant' AND EXISTS(SELECT 1 FROM assistant_messages a WHERE a.id=NEW.source_id AND a.codex_thread_id=r.thread_id AND a.codex_turn_id=r.turn_id))
   OR (NEW.source='message' AND EXISTS(SELECT 1 FROM messages m WHERE m.id=NEW.source_id AND m.codex_thread_id=r.thread_id AND m.codex_turn_id=r.turn_id))
  ));
END;
