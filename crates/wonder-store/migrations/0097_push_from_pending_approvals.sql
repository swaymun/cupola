-- Approval notifications come from the pending request itself, one per
-- request, so they match the Permission/Question list the phone shows. The
-- event-based intent missed requests whose turn was not yet mapped to a
-- conversation (the event then had no conversation) and requests recorded
-- without an `approval_opened` event. The conversation is resolved when the
-- intent is distributed; an empty value means "not resolved yet".
DROP TRIGGER push_event;
CREATE TRIGGER push_event AFTER INSERT ON events
WHEN EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)
 AND json_extract(NEW.payload_json,'$.conversationId') IS NOT NULL
 AND json_extract(NEW.payload_json,'$.event.type')='message_state'
 AND json_extract(NEW.payload_json,'$.event.data.state') IN ('completed','failed')
BEGIN
 INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind)
 VALUES(NEW.event_id,json_extract(NEW.payload_json,'$.conversationId'),
 CASE WHEN json_extract(NEW.payload_json,'$.event.data.state')='completed' THEN 'completed' ELSE 'attention' END);
END;
CREATE TRIGGER push_approval AFTER INSERT ON approvals
WHEN NEW.state='pending'
 AND EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)

BEGIN
 INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind)
 VALUES('approval:'||NEW.server_request_id,'','attention');
END;
