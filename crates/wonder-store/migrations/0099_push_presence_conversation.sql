-- The phone reports which conversation is open while in the foreground, so
-- only that conversation stays quiet. NULL is an older phone that reports no
-- conversation: everything on this Mac stays quiet while it is in front, as
-- before. '' is a newer phone with no conversation open.
ALTER TABLE push_presence ADD COLUMN conversation_id TEXT;
DROP TRIGGER push_foreground_intent;
CREATE TRIGGER push_foreground_intent AFTER INSERT ON push_intents BEGIN
 INSERT OR IGNORE INTO push_suppressed(intent_id,device_id)
 SELECT NEW.id,device_id FROM push_presence
 WHERE foreground_until_ms>unixepoch('subsec')*1000
 AND (conversation_id IS NULL OR (NEW.conversation_id!='' AND conversation_id=NEW.conversation_id));
END;
