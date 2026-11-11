-- A Project thread keeps its id, history and queue while its provider changes.
-- The change is decided when a message is released for delivery, from the model
-- that message carries, and applied only by Store::switch_project_runtime. The
-- operation writes a row here for the length of its transaction; the identity
-- triggers below let the provider columns change only while that row exists,
-- so no ordinary write can move a thread to the other provider.
CREATE TABLE project_runtime_switch_guard (
 conversation_id TEXT PRIMARY KEY NOT NULL
);

-- One row per native session a thread has left and not resumed, oldest first.
CREATE TABLE project_runtime_history (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 provider_store TEXT NOT NULL,
 native_session_id TEXT,
 runtime_thread_id TEXT,
 model TEXT,
 switched_at TEXT NOT NULL,
 -- How many readable history entries the thread had when it left, when known.
 last_position INTEGER CHECK(last_position IS NULL OR last_position >= 0)
);
CREATE INDEX project_runtime_history_conversation ON project_runtime_history(conversation_id, id);

-- Set by a switch, cleared once the provider accepts the first turn that
-- carries the handed-over context. A resumed session receives only what
-- happened after history entry `delta_after_history_id`.
CREATE TABLE project_pending_handoff (
 conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 history_id INTEGER NOT NULL,
 delta_after_history_id INTEGER,
 created_at TEXT NOT NULL
);

-- Whether the context handoff reached a native session. `pending` is written
-- before the turn is submitted and `injected` once the provider accepted it;
-- a pending row that never became injected is ambiguous, so that session is
-- replaced instead of being given the context a second time.
CREATE TABLE project_handoff_delivery (
 conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 native_thread_id TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','injected')),
 updated_at TEXT NOT NULL,
 PRIMARY KEY(conversation_id, native_thread_id)
);

-- The model a message carries, which may belong to the other provider. It is
-- staged just before the message is accepted and consumed in that same
-- transaction, so the message's frozen settings can never miss it.
CREATE TABLE project_message_targets (
 device_id TEXT NOT NULL,
 client_message_id TEXT NOT NULL,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 model TEXT NOT NULL,
 effort TEXT,
 service_tier TEXT,
 created_at TEXT NOT NULL,
 PRIMARY KEY(device_id, client_message_id)
);

-- The provider a message was accepted for. Older messages have none and use
-- the thread's provider at delivery.
ALTER TABLE message_execution_settings ADD COLUMN agent_family TEXT
 CHECK(agent_family IS NULL OR agent_family IN ('codex','claude','opencode'));

DROP TRIGGER project_conversation_identity_is_fixed;
CREATE TRIGGER project_conversation_identity_is_fixed BEFORE UPDATE OF
 project_id, agent_family, provider_store, cwd ON project_conversations
WHEN (NEW.project_id != OLD.project_id OR NEW.cwd != OLD.cwd
 OR ((NEW.agent_family != OLD.agent_family OR NEW.provider_store != OLD.provider_store)
  AND NOT EXISTS(SELECT 1 FROM project_runtime_switch_guard WHERE conversation_id=OLD.conversation_id)))
BEGIN
 SELECT RAISE(ABORT,'A project conversation keeps its provider identity and working folder');
END;

DROP TRIGGER project_native_session_is_fixed;
CREATE TRIGGER project_native_session_is_fixed BEFORE UPDATE OF native_session_id ON project_conversations
WHEN OLD.native_session_id IS NOT NULL AND NEW.native_session_id IS NOT OLD.native_session_id
 AND NOT EXISTS(SELECT 1 FROM project_runtime_switch_guard WHERE conversation_id=OLD.conversation_id)
BEGIN
 SELECT RAISE(ABORT,'A project conversation cannot move to another native session');
END;
