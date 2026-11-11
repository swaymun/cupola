-- Canonical history references survive replay expiry. Sequence is a stable
-- tie-breaker; timestamp normalization supports both legacy milliseconds and RFC3339.
CREATE TABLE history_entries (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 conversation_id TEXT NOT NULL,
 source TEXT NOT NULL,
 source_id TEXT NOT NULL,
 sort_ms INTEGER NOT NULL,
 payload_json TEXT,
 UNIQUE(source, source_id)
);
CREATE INDEX history_conversation_page ON history_entries(conversation_id, sort_ms DESC, sequence DESC);
CREATE INDEX history_runtime_client ON history_entries(conversation_id, json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.clientId')) WHERE source='event';
CREATE TABLE history_hydration (
 conversation_id TEXT PRIMARY KEY,
 state TEXT NOT NULL DEFAULT 'idle',
 updated_at_ms INTEGER NOT NULL DEFAULT 0,
 detail TEXT,
 token TEXT
);
INSERT INTO history_entries(conversation_id, source, source_id, sort_ms)
 SELECT conversation_id, source, id, stamp FROM (
 SELECT conversation_id, 'message' AS source, id, CASE WHEN created_at NOT GLOB '*[^0-9]*' AND created_at != '' THEN CAST(created_at AS INTEGER) ELSE COALESCE(CAST((julianday(created_at) - 2440587.5) * 86400000 AS INTEGER), 0) END AS stamp FROM messages
 UNION ALL SELECT conversation_id, 'assistant', id, CASE WHEN created_at NOT GLOB '*[^0-9]*' AND created_at != '' THEN CAST(created_at AS INTEGER) ELSE COALESCE(CAST((julianday(created_at) - 2440587.5) * 86400000 AS INTEGER), 0) END FROM assistant_messages
 ) ORDER BY stamp, source, id;
CREATE TRIGGER history_messages_insert AFTER INSERT ON messages BEGIN
 INSERT INTO history_entries(conversation_id, source, source_id, sort_ms)
 VALUES (NEW.conversation_id, 'message', NEW.id, CASE WHEN NEW.created_at NOT GLOB '*[^0-9]*' AND NEW.created_at != '' THEN CAST(NEW.created_at AS INTEGER) ELSE COALESCE(CAST((julianday(NEW.created_at) - 2440587.5) * 86400000 AS INTEGER), 0) END);
 DELETE FROM history_entries WHERE source='event' AND conversation_id=NEW.conversation_id AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.clientId')=NEW.client_message_id;
END;
CREATE TRIGGER history_messages_delete AFTER DELETE ON messages BEGIN
 DELETE FROM history_entries WHERE source = 'message' AND source_id = OLD.id;
END;
CREATE TRIGGER history_assistant_messages_insert AFTER INSERT ON assistant_messages BEGIN
 INSERT INTO history_entries(conversation_id, source, source_id, sort_ms)
 VALUES (NEW.conversation_id, 'assistant', NEW.id, CASE WHEN NEW.created_at NOT GLOB '*[^0-9]*' AND NEW.created_at != '' THEN CAST(NEW.created_at AS INTEGER) ELSE COALESCE(CAST((julianday(NEW.created_at) - 2440587.5) * 86400000 AS INTEGER), 0) END);
 DELETE FROM history_entries WHERE source='event' AND source_id=NEW.conversation_id || ':' || NEW.codex_thread_id || ':' || NEW.codex_turn_id || ':' || NEW.item_id;
END;
CREATE TRIGGER history_assistant_messages_delete AFTER DELETE ON assistant_messages BEGIN
 DELETE FROM history_entries WHERE source = 'assistant' AND source_id = OLD.id;
END;
-- Conversation lookups no longer deserialize every epoch's replay events.
CREATE INDEX events_conversation_sequence ON events(json_extract(payload_json, '$.conversationId'), sequence);
CREATE INDEX journal_epoch_sequence ON sync_journal(host_epoch, sequence);
DROP TRIGGER sync_journal_committed_head;
CREATE TRIGGER sync_journal_committed_head AFTER INSERT ON sync_journal BEGIN
 UPDATE sync_state SET last_sequence = NEW.sequence WHERE singleton = 1;
END;
-- One global ledger accounts for both stored replay copies, across every epoch.
CREATE TABLE replay_retention (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 source TEXT NOT NULL,
 source_id TEXT NOT NULL,
 retained_at INTEGER NOT NULL,
 payload_bytes INTEGER NOT NULL,
 UNIQUE(source, source_id)
);
CREATE TABLE replay_usage(singleton INTEGER PRIMARY KEY CHECK(singleton=1), event_count INTEGER NOT NULL, payload_bytes INTEGER NOT NULL);
INSERT INTO replay_usage VALUES(1,0,0);
CREATE INDEX replay_retention_age ON replay_retention(retained_at, id);
CREATE TRIGGER replay_usage_insert AFTER INSERT ON replay_retention BEGIN
 UPDATE replay_usage SET event_count=event_count+1, payload_bytes=payload_bytes+NEW.payload_bytes WHERE singleton=1;
END;
CREATE TRIGGER replay_usage_update AFTER UPDATE OF payload_bytes ON replay_retention BEGIN
 UPDATE replay_usage SET payload_bytes=payload_bytes+NEW.payload_bytes-OLD.payload_bytes WHERE singleton=1;
END;
CREATE TRIGGER replay_usage_delete AFTER DELETE ON replay_retention BEGIN
 UPDATE replay_usage SET event_count=event_count-1, payload_bytes=payload_bytes-OLD.payload_bytes WHERE singleton=1;
 DELETE FROM events WHERE OLD.source='events' AND event_id=OLD.source_id;
 DELETE FROM sync_journal WHERE OLD.source='journal' AND sequence=CAST(OLD.source_id AS INTEGER);
END;
CREATE TRIGGER replay_events_insert AFTER INSERT ON events BEGIN
 INSERT INTO replay_retention(source,source_id,retained_at,payload_bytes) VALUES('events',NEW.event_id,unixepoch(),length(CAST(NEW.payload_json AS BLOB)));
END;
CREATE TRIGGER replay_events_update AFTER UPDATE OF payload_json ON events BEGIN
 UPDATE replay_retention SET payload_bytes=length(CAST(NEW.payload_json AS BLOB)) WHERE source='events' AND source_id=CAST(NEW.event_id AS TEXT);
END;
CREATE TRIGGER replay_events_delete AFTER DELETE ON events BEGIN
 DELETE FROM replay_retention WHERE source='events' AND source_id=CAST(OLD.event_id AS TEXT);
END;
INSERT INTO replay_retention(source,source_id,retained_at,payload_bytes) SELECT 'events',event_id,CASE WHEN occurred_at != '' AND occurred_at NOT GLOB '*[^0-9]*' THEN CAST(occurred_at AS INTEGER)/1000 ELSE COALESCE(unixepoch(occurred_at),unixepoch()) END,length(CAST(payload_json AS BLOB)) FROM events ORDER BY event_id;
CREATE TRIGGER replay_journal_insert AFTER INSERT ON sync_journal BEGIN
 INSERT INTO replay_retention(source,source_id,retained_at,payload_bytes) VALUES('journal',NEW.sequence,unixepoch(),COALESCE(length(CAST(NEW.payload_json AS BLOB)), 512 + length(COALESCE(NEW.conversation_id,'')) + length(COALESCE(NEW.resource,'')) + length(COALESCE(NEW.change_kind,''))));
END;
CREATE TRIGGER replay_journal_update AFTER UPDATE OF payload_json ON sync_journal BEGIN
 UPDATE replay_retention SET payload_bytes=COALESCE(length(CAST(NEW.payload_json AS BLOB)), 512 + length(COALESCE(NEW.conversation_id,'')) + length(COALESCE(NEW.resource,'')) + length(COALESCE(NEW.change_kind,''))) WHERE source='journal' AND source_id=CAST(NEW.sequence AS TEXT);
END;
CREATE TRIGGER replay_journal_delete AFTER DELETE ON sync_journal BEGIN
 DELETE FROM replay_retention WHERE source='journal' AND source_id=CAST(OLD.sequence AS TEXT);
END;
INSERT INTO replay_retention(source,source_id,retained_at,payload_bytes) SELECT 'journal',sequence,CASE WHEN occurred_at != '' AND occurred_at NOT GLOB '*[^0-9]*' THEN CAST(occurred_at AS INTEGER)/1000 ELSE COALESCE(unixepoch(occurred_at),unixepoch()) END,COALESCE(length(CAST(payload_json AS BLOB)), 512 + length(COALESCE(conversation_id,'')) + length(COALESCE(resource,'')) + length(COALESCE(change_kind,''))) FROM sync_journal ORDER BY sequence;

-- Preserve retained typed activities during the one-time upgrade. Full message
-- bodies already have canonical rows; replay deltas are not transcript items.
INSERT INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json)
SELECT conversation_id, 'event', source_id, sort_ms, payload_json FROM (
 SELECT *, row_number() OVER (PARTITION BY source_id ORDER BY sequence DESC) AS latest FROM (
  SELECT json_extract(payload_json,'$.conversationId') AS conversation_id,
   CASE WHEN json_extract(payload_json,'$.event.data.category')='thread_item_upsert'
    THEN json_extract(payload_json,'$.conversationId') || ':' || COALESCE(json_extract(payload_json,'$.threadId'),'') || ':' || COALESCE(json_extract(json_extract(payload_json,'$.event.data.detail'),'$.turnId'),'') || ':' || COALESCE(json_extract(json_extract(payload_json,'$.event.data.detail'),'$.itemId'),event_id)
    ELSE event_id END AS source_id,
   CASE WHEN occurred_at NOT GLOB '*[^0-9]*' AND occurred_at != '' THEN CAST(occurred_at AS INTEGER) ELSE COALESCE(CAST((julianday(occurred_at)-2440587.5)*86400000 AS INTEGER),0) END AS sort_ms,
   payload_json, sequence
  FROM events WHERE json_extract(payload_json,'$.conversationId') IS NOT NULL AND
   (json_extract(payload_json,'$.event.type')='computer_use_screenshot' OR
    (json_extract(payload_json,'$.event.data.category')='thread_item_upsert' AND json_valid(json_extract(payload_json,'$.event.data.detail'))))
 )
) AS history WHERE latest=1
 AND NOT EXISTS (SELECT 1 FROM assistant_messages a WHERE a.conversation_id=history.conversation_id AND a.codex_turn_id=json_extract(json_extract(history.payload_json,'$.event.data.detail'),'$.turnId') AND a.item_id=json_extract(json_extract(history.payload_json,'$.event.data.detail'),'$.itemId'))
 AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id=history.conversation_id AND m.client_message_id=json_extract(json_extract(history.payload_json,'$.event.data.detail'),'$.item.clientId'))
 ORDER BY sort_ms,sequence;
