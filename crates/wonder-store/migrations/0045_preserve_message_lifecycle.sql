-- Text arrival must not erase commentary/final phase from the typed item.
DROP TRIGGER history_assistant_messages_insert;
CREATE TRIGGER history_assistant_messages_insert AFTER INSERT ON assistant_messages BEGIN
 INSERT INTO history_entries(conversation_id, source, source_id, sort_ms)
 VALUES (NEW.conversation_id, 'assistant', NEW.id, CASE WHEN NEW.created_at NOT GLOB '*[^0-9]*' AND NEW.created_at != '' THEN CAST(NEW.created_at AS INTEGER) ELSE COALESCE(CAST((julianday(NEW.created_at) - 2440587.5) * 86400000 AS INTEGER), 0) END);
END;
-- Recover phase metadata still present in the bounded replay log.
INSERT OR IGNORE INTO history_entries(conversation_id,source,source_id,sort_ms,payload_json)
SELECT conversation_id,'event',source_id,sort_ms,payload_json FROM (
 SELECT *,row_number() OVER(PARTITION BY source_id ORDER BY sequence DESC) AS latest FROM (
  SELECT json_extract(payload_json,'$.conversationId') AS conversation_id,
   json_extract(payload_json,'$.conversationId') || ':' || COALESCE(json_extract(payload_json,'$.threadId'),'') || ':' || json_extract(json_extract(payload_json,'$.event.data.detail'),'$.turnId') || ':' || json_extract(json_extract(payload_json,'$.event.data.detail'),'$.itemId') AS source_id,
   CASE WHEN occurred_at NOT GLOB '*[^0-9]*' AND occurred_at != '' THEN CAST(occurred_at AS INTEGER) ELSE COALESCE(CAST((julianday(occurred_at)-2440587.5)*86400000 AS INTEGER),0) END AS sort_ms,
   payload_json,sequence
  FROM events WHERE json_extract(payload_json,'$.event.data.category')='thread_item_upsert'
   AND json_valid(json_extract(payload_json,'$.event.data.detail'))
   AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.type')='agentMessage'
   AND json_extract(json_extract(payload_json,'$.event.data.detail'),'$.item.phase') IS NOT NULL
 ) WHERE conversation_id IS NOT NULL AND source_id IS NOT NULL
) WHERE latest=1;
