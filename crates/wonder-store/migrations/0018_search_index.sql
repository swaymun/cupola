CREATE VIRTUAL TABLE IF NOT EXISTS search_documents USING fts5(
    kind UNINDEXED,
    result_id UNINDEXED,
    title,
    body,
    conversation_id UNINDEXED,
    updated_at UNINDEXED
);
