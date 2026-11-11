-- All conversation lookup and durable delivery intent stay on this Mac.
CREATE TABLE push_registrations (
 id TEXT PRIMARY KEY, device_id TEXT NOT NULL REFERENCES devices(id),
 endpoint TEXT NOT NULL, sender_secret TEXT NOT NULL,
 revoked INTEGER NOT NULL DEFAULT 0, revoke_attempts INTEGER NOT NULL DEFAULT 0,
 next_revoke INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX push_active_device ON push_registrations(device_id) WHERE revoked=0;
CREATE TABLE push_intents (
 id INTEGER PRIMARY KEY AUTOINCREMENT, source_key TEXT NOT NULL UNIQUE,
 conversation_id TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('completed','attention')),
 created_at INTEGER NOT NULL DEFAULT (unixepoch()), distributed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE push_routes (id TEXT PRIMARY KEY, device_id TEXT NOT NULL REFERENCES devices(id), conversation_id TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()));
CREATE TABLE push_outbox (
 id TEXT PRIMARY KEY, registration_id TEXT NOT NULL REFERENCES push_registrations(id) ON DELETE CASCADE,
 intent_id INTEGER NOT NULL REFERENCES push_intents(id) ON DELETE CASCADE,
 route_id TEXT NOT NULL UNIQUE, conversation_id TEXT NOT NULL, kind TEXT NOT NULL,
 attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0,
 state TEXT NOT NULL DEFAULT 'pending', created_at INTEGER NOT NULL DEFAULT (unixepoch()),
 UNIQUE(registration_id,intent_id)
);
CREATE INDEX push_due ON push_outbox(state,next_attempt);
-- Commit completion/attention intent in the same transaction as the event.
CREATE TRIGGER push_event AFTER INSERT ON events
WHEN EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)
 AND json_extract(NEW.payload_json,'$.conversationId') IS NOT NULL
 AND (json_extract(NEW.payload_json,'$.event.type')='approval_opened'
 OR (json_extract(NEW.payload_json,'$.event.type')='message_state'
 AND json_extract(NEW.payload_json,'$.event.data.state') IN ('completed','failed')))
BEGIN
 INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind)
 VALUES(NEW.event_id,json_extract(NEW.payload_json,'$.conversationId'),
 CASE WHEN json_extract(NEW.payload_json,'$.event.data.state')='completed' THEN 'completed' ELSE 'attention' END);
END;
CREATE TRIGGER push_question AFTER INSERT ON async_questions
WHEN NEW.state='pending' AND EXISTS(SELECT 1 FROM push_registrations WHERE revoked=0)
BEGIN
 INSERT OR IGNORE INTO push_intents(source_key,conversation_id,kind)
 VALUES('question:'||NEW.id,NEW.conversation_id,'attention');
END;
CREATE TRIGGER push_revoke_device AFTER UPDATE OF revoked_at ON devices
WHEN NEW.revoked_at IS NOT NULL
BEGIN
 UPDATE push_registrations SET revoked=1 WHERE device_id=NEW.id;
 UPDATE push_outbox SET state='cancelled' WHERE registration_id IN (SELECT id FROM push_registrations WHERE device_id=NEW.id) AND state='pending';
END;
