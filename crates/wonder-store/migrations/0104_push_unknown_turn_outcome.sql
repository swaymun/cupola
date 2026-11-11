-- A turn that ended with an outcome Wonder cannot read (a status outside the
-- provider's known set) is recorded as `uncertain` on that turn. It is not a
-- finished reply, so it asks for attention instead of announcing completion.
-- An uncertain receipt without a turn (a send whose delivery is unknown) is
-- unchanged and sends nothing.
DROP TRIGGER push_event;
CREATE TRIGGER push_event AFTER INSERT ON events
WHEN EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)
 AND json_extract(NEW.payload_json,'$.conversationId') IS NOT NULL
 AND json_extract(NEW.payload_json,'$.event.type')='message_state'
 AND (json_extract(NEW.payload_json,'$.event.data.state') IN ('completed','failed')
 OR (json_extract(NEW.payload_json,'$.event.data.state')='uncertain'
 AND COALESCE(json_extract(NEW.payload_json,'$.turnId'),'')!=''))
BEGIN
 INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind)
 VALUES(NEW.event_id,json_extract(NEW.payload_json,'$.conversationId'),
 CASE WHEN json_extract(NEW.payload_json,'$.event.data.state')='completed' THEN 'completed' ELSE 'attention' END);
END;
