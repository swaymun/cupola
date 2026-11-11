-- Provider-neutral authority for routing. Historical codex_* storage columns
-- are retained by the provider-neutral transcript and receipt projections.
CREATE TABLE runtime_bindings (
 conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 runtime_thread_id TEXT NOT NULL,
 session_id TEXT,
 execution_scope TEXT NOT NULL CHECK(execution_scope = 'projects'),
 provider_store TEXT NOT NULL,
 updated_at TEXT NOT NULL
);
-- A Claude session is exclusive to its conversation.
CREATE UNIQUE INDEX claude_runtime_thread_owner ON runtime_bindings(runtime_thread_id) WHERE agent_family='claude';
CREATE INDEX runtime_binding_thread ON runtime_bindings(runtime_thread_id);
CREATE TRIGGER runtime_binding_family_is_fixed BEFORE UPDATE OF agent_family ON runtime_bindings
WHEN NEW.agent_family != OLD.agent_family
BEGIN
 SELECT RAISE(ABORT,'A runtime session cannot move between agent families');
END;
