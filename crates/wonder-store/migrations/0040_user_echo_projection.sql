-- Repair assistant projections whose retained typed event proves they were user
-- items. Canonical messages and events remain unchanged. Equal text alone never
-- identifies an echo (an agent may deliberately repeat what the user said).
DELETE FROM assistant_messages
WHERE EXISTS (
 SELECT 1 FROM events
 WHERE json_extract(payload_json, '$.conversationId') = assistant_messages.conversation_id
 AND json_extract(payload_json, '$.threadId') = assistant_messages.codex_thread_id
 AND json_extract(payload_json, '$.turnId') = assistant_messages.codex_turn_id
 AND json_extract(payload_json, '$.itemId') = assistant_messages.item_id
 AND json_extract(payload_json, '$.event.data.category') = 'thread_item_upsert'
 AND json_extract(json_extract(payload_json, '$.event.data.detail'), '$.item.type') = 'userMessage'
);
