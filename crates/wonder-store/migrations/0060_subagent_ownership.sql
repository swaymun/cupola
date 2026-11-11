CREATE TABLE subagent_ownership (
    conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id),
    parent_conversation_id TEXT NOT NULL REFERENCES conversations(id),
    thread_id TEXT NOT NULL UNIQUE,
    parent_thread_id TEXT NOT NULL,
    agent_nickname TEXT,
    agent_role TEXT,
    agent_path TEXT,
    source_json TEXT NOT NULL,
    runtime_id TEXT,
    can_accept_direct_input INTEGER,
    status TEXT NOT NULL DEFAULT 'active',
    is_archived INTEGER NOT NULL DEFAULT 0,
    verified_at TEXT NOT NULL
);

CREATE INDEX subagent_ownership_parent_idx
    ON subagent_ownership(parent_conversation_id, is_archived, conversation_id);

CREATE INDEX subagent_ownership_thread_idx
    ON subagent_ownership(thread_id, parent_thread_id);
