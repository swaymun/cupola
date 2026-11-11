-- A derived projection, not delivery state. The first read creates a row;
-- later history edits/reset/deletion invalidate only conversations with one.
CREATE TABLE t3_echo_cache (
 conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
 revision INTEGER NOT NULL DEFAULT 0,
 signature TEXT,
 last_sequence INTEGER NOT NULL DEFAULT 0,
 matches_json TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX history_t3_user_sequence ON history_entries(conversation_id,sequence)
 WHERE source='event' AND hidden_by_reset=0 AND instr(payload_json,'userMessage')>0
 AND json_type(payload_json,'$.event.data.detail')='text'
 AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.type')='userMessage'
 AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.clientId') IS NULL;
CREATE TRIGGER t3_echo_update AFTER UPDATE OF payload_json,hidden_by_reset,conversation_id,source,sequence ON history_entries
 WHEN (OLD.source='event' AND instr(OLD.payload_json,'userMessage')>0 AND json_type(OLD.payload_json,'$.event.data.detail')='text' AND json_extract(json_extract(OLD.payload_json,'$.event.data.detail'),'$.item.type')='userMessage'
    OR NEW.source='event' AND instr(NEW.payload_json,'userMessage')>0 AND json_type(NEW.payload_json,'$.event.data.detail')='text' AND json_extract(json_extract(NEW.payload_json,'$.event.data.detail'),'$.item.type')='userMessage')
 AND (OLD.payload_json IS NOT NEW.payload_json OR OLD.hidden_by_reset IS NOT NEW.hidden_by_reset OR OLD.conversation_id IS NOT NEW.conversation_id OR OLD.source IS NOT NEW.source OR OLD.sequence IS NOT NEW.sequence)
 BEGIN UPDATE t3_echo_cache SET revision=revision+1,signature=NULL,last_sequence=0,matches_json='[]'
 WHERE conversation_id IN (OLD.conversation_id,NEW.conversation_id); END;
CREATE TRIGGER t3_echo_delete AFTER DELETE ON history_entries
 WHEN OLD.source='event' AND instr(OLD.payload_json,'userMessage')>0 AND json_type(OLD.payload_json,'$.event.data.detail')='text' AND json_extract(json_extract(OLD.payload_json,'$.event.data.detail'),'$.item.type')='userMessage'
 BEGIN UPDATE t3_echo_cache SET revision=revision+1,signature=NULL,last_sequence=0,matches_json='[]'
 WHERE conversation_id=OLD.conversation_id; END;
-- Normal appended rows are read incrementally. An explicit older insertion
-- (e.g. an import) must reconsider earlier greedy matches.
CREATE TRIGGER t3_echo_insert AFTER INSERT ON history_entries
 WHEN NEW.source='event' AND instr(NEW.payload_json,'userMessage')>0 AND json_type(NEW.payload_json,'$.event.data.detail')='text' AND json_extract(json_extract(NEW.payload_json,'$.event.data.detail'),'$.item.type')='userMessage'
 BEGIN UPDATE t3_echo_cache SET revision=revision+1,signature=NULL,last_sequence=0,matches_json='[]'
 WHERE conversation_id=NEW.conversation_id AND NEW.sequence<=last_sequence; END;
