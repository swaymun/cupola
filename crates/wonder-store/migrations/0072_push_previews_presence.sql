-- Preview secrets stay on the paired Mac and in the phone's shared Keychain.
ALTER TABLE push_registrations ADD COLUMN preview_key TEXT;
CREATE TABLE push_presence (
 device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
 issued_at_ms INTEGER NOT NULL,
 foreground_until_ms INTEGER NOT NULL
);
CREATE TABLE push_suppressed (
 intent_id INTEGER NOT NULL REFERENCES push_intents(id) ON DELETE CASCADE,
 device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
 PRIMARY KEY(intent_id,device_id)
);
-- Remember foreground arrivals even if the app closes before distribution.
CREATE TRIGGER push_foreground_intent AFTER INSERT ON push_intents BEGIN
 INSERT OR IGNORE INTO push_suppressed(intent_id,device_id)
 SELECT NEW.id,device_id FROM push_presence
 WHERE foreground_until_ms>unixepoch('subsec')*1000;
END;
