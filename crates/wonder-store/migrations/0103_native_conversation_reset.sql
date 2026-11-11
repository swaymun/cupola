-- Reset changes the visible epoch of the existing canonical history index.
-- Delivery receipts and native transcript files remain intact.
ALTER TABLE conversations ADD COLUMN history_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN last_native_reset_session_id TEXT;
ALTER TABLE history_entries ADD COLUMN hidden_by_reset INTEGER NOT NULL DEFAULT 0;
CREATE INDEX history_visible_page ON history_entries(conversation_id, hidden_by_reset, sort_ms DESC, sequence DESC);
CREATE INDEX history_hidden_turn ON history_entries(conversation_id, json_extract(payload_json,'$.threadId'), json_extract(payload_json,'$.turnId')) WHERE hidden_by_reset=1 AND source='event';

-- A late assistant row belongs to the same turn as its retained receipt. Its
-- insertion cannot make an old turn visible after reset.
CREATE TRIGGER history_reset_late_assistant AFTER INSERT ON history_entries
WHEN NEW.source='assistant' BEGIN
 UPDATE history_entries SET hidden_by_reset=1 WHERE sequence=NEW.sequence AND EXISTS (
  SELECT 1 FROM assistant_messages a JOIN messages m ON m.conversation_id=a.conversation_id AND m.codex_thread_id=a.codex_thread_id AND m.codex_turn_id=a.codex_turn_id
  JOIN history_entries h ON h.source='message' AND h.source_id=m.id
  WHERE a.id=NEW.source_id AND h.hidden_by_reset=1)
 OR sequence=NEW.sequence AND EXISTS (SELECT 1 FROM assistant_messages a JOIN history_entries h ON h.conversation_id=a.conversation_id
  AND h.source='event' AND h.hidden_by_reset=1 AND json_extract(h.payload_json,'$.threadId')=a.codex_thread_id
  AND json_extract(h.payload_json,'$.turnId')=a.codex_turn_id WHERE a.id=NEW.source_id);
END;
CREATE TRIGGER history_reset_late_event AFTER INSERT ON history_entries
WHEN NEW.source='event' BEGIN
 UPDATE history_entries SET hidden_by_reset=1 WHERE sequence=NEW.sequence AND (
  EXISTS (SELECT 1 FROM history_entries h WHERE h.conversation_id=NEW.conversation_id AND h.source='event' AND h.hidden_by_reset=1
   AND json_extract(h.payload_json,'$.threadId')=json_extract(NEW.payload_json,'$.threadId')
   AND json_extract(h.payload_json,'$.turnId')=json_extract(NEW.payload_json,'$.turnId'))
  OR EXISTS (SELECT 1 FROM messages m JOIN history_entries h ON h.source='message' AND h.source_id=m.id
   WHERE m.conversation_id=NEW.conversation_id AND m.codex_thread_id=json_extract(NEW.payload_json,'$.threadId')
   AND m.codex_turn_id=json_extract(NEW.payload_json,'$.turnId') AND h.hidden_by_reset=1));
END;
