-- Register Cursor without changing any applied migration. SQLite cannot widen
-- a CHECK in place. Preserve the affected tables and their dependent rows,
-- then rebuild in foreign-key order inside the migrator's single transaction.
-- Foreign keys stay enabled; no writable_schema or connection-local bypass.


CREATE TEMP TABLE cursor_backup_agent_family_settings AS SELECT * FROM agent_family_settings;

CREATE TEMP TABLE cursor_backup_message_execution_settings AS SELECT * FROM message_execution_settings;

CREATE TEMP TABLE cursor_backup_projects AS SELECT * FROM projects;

CREATE TEMP TABLE cursor_backup_project_conversations AS SELECT * FROM project_conversations;

CREATE TEMP TABLE cursor_backup_project_conversation_forks AS SELECT * FROM project_conversation_forks;

CREATE TEMP TABLE cursor_backup_project_handoff_delivery AS SELECT * FROM project_handoff_delivery;

CREATE TEMP TABLE cursor_backup_project_message_targets AS SELECT * FROM project_message_targets;

CREATE TEMP TABLE cursor_backup_project_pending_handoff AS SELECT * FROM project_pending_handoff;

CREATE TEMP TABLE cursor_backup_provider_accounts AS SELECT * FROM provider_accounts;

CREATE TEMP TABLE cursor_backup_project_provider_accounts AS SELECT * FROM project_provider_accounts;

CREATE TEMP TABLE cursor_backup_project_provider_refs AS SELECT * FROM project_provider_refs;

CREATE TEMP TABLE cursor_backup_project_roots AS SELECT * FROM project_roots;

CREATE TEMP TABLE cursor_backup_project_runtime_history AS SELECT * FROM project_runtime_history;

CREATE TEMP TABLE cursor_backup_provider_settings AS SELECT * FROM provider_settings;

CREATE TEMP TABLE cursor_backup_runtime_bindings AS SELECT * FROM runtime_bindings;

CREATE TEMP TABLE cursor_backup_thread_delegations AS SELECT * FROM thread_delegations;

CREATE TEMP TABLE cursor_backup_sequence AS SELECT name,seq FROM sqlite_sequence;

DROP TRIGGER runtime_binding_family_is_fixed;

DROP TRIGGER project_model_stays_in_family;

DROP TRIGGER runtime_binding_scope_is_fixed;

DROP TRIGGER project_conversation_identity_is_fixed;

DROP TRIGGER project_native_session_is_fixed;

DROP TABLE thread_delegations;

DROP TABLE runtime_bindings;

DROP TABLE provider_settings;

DROP TABLE project_runtime_history;

DROP TABLE project_roots;

DROP TABLE project_provider_refs;

DROP TABLE project_provider_accounts;

DROP TABLE provider_accounts;

DROP TABLE project_pending_handoff;

DROP TABLE project_message_targets;

DROP TABLE project_handoff_delivery;

DROP TABLE project_conversation_forks;

DROP TABLE project_conversations;

DROP TABLE projects;

DROP TABLE message_execution_settings;

DROP TABLE agent_family_settings;

CREATE TABLE agent_family_settings (
  agent_family TEXT PRIMARY KEY NOT NULL CHECK (agent_family IN ('codex','claude','opencode','cursor')),
  thread_tools INTEGER NOT NULL DEFAULT 1 CHECK (thread_tools IN (0,1)),
  chat_detail TEXT NOT NULL DEFAULT 'everything' CHECK (chat_detail IN ('everything','conversation')),
  instruction_enabled INTEGER NOT NULL DEFAULT 0 CHECK (instruction_enabled IN (0,1)),
  instruction TEXT NOT NULL DEFAULT '' CHECK (length(instruction) <= 4000),
  updated_at TEXT NOT NULL
);

INSERT INTO agent_family_settings SELECT * FROM cursor_backup_agent_family_settings;

DROP TABLE cursor_backup_agent_family_settings;

CREATE TABLE message_execution_settings (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    model TEXT, reasoning_effort TEXT, service_tier TEXT,
    working_directory TEXT
, agent_family TEXT
 CHECK(agent_family IS NULL OR agent_family IN ('codex','claude','opencode','cursor')));

INSERT INTO message_execution_settings SELECT * FROM cursor_backup_message_execution_settings;

DROP TABLE cursor_backup_message_execution_settings;

CREATE TABLE projects (
 id TEXT PRIMARY KEY,
 name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 120),
 is_included INTEGER NOT NULL DEFAULT 1 CHECK(is_included IN (0,1)),
 pin_order INTEGER,
 primary_root_id TEXT NOT NULL,
 roots_revision INTEGER NOT NULL DEFAULT 1 CHECK(roots_revision >= 1),
 last_family TEXT CHECK(last_family IN ('codex','claude','opencode','cursor')),
 creation_request_id TEXT UNIQUE,
 creation_payload_sha256 TEXT,
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 last_used_at TEXT
);

INSERT INTO projects SELECT * FROM cursor_backup_projects;

DROP TABLE cursor_backup_projects;

CREATE TABLE project_conversations (
 conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
 project_id TEXT NOT NULL REFERENCES projects(id),
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 provider_store TEXT NOT NULL,
 native_session_id TEXT,
 cwd TEXT NOT NULL,
 roots_revision INTEGER NOT NULL,
 title TEXT NOT NULL,
 model TEXT,
 effort TEXT,
 access_mode TEXT NOT NULL DEFAULT 'workspace' CHECK(access_mode IN ('read_only','workspace','full_access')),
 is_pinned INTEGER NOT NULL DEFAULT 0 CHECK(is_pinned IN (0,1)),
 has_unread INTEGER NOT NULL DEFAULT 0 CHECK(has_unread IN (0,1)),
 creation_request_id TEXT UNIQUE,
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 last_activity_at TEXT NOT NULL, claude_approval TEXT NOT NULL DEFAULT 'ask'
 CHECK(claude_approval IN ('ask','accept_edits','auto')), plan_mode INTEGER NOT NULL DEFAULT 0
 CHECK(plan_mode IN (0,1)), creation_payload TEXT, service_tier TEXT, native_settings_turn TEXT, wonder_archived_at TEXT,
 CHECK(agent_family NOT IN ('opencode','cursor') OR access_mode = 'workspace')
);

INSERT INTO project_conversations SELECT * FROM cursor_backup_project_conversations;

DROP TABLE cursor_backup_project_conversations;

CREATE TABLE project_conversation_forks (
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  forked_from TEXT NOT NULL,
  forked_at_turn TEXT
);

INSERT INTO project_conversation_forks SELECT * FROM cursor_backup_project_conversation_forks;

DROP TABLE cursor_backup_project_conversation_forks;

CREATE TABLE project_handoff_delivery (
 conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 native_thread_id TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','injected')),
 updated_at TEXT NOT NULL,
 PRIMARY KEY(conversation_id, native_thread_id)
);

INSERT INTO project_handoff_delivery SELECT * FROM cursor_backup_project_handoff_delivery;

DROP TABLE cursor_backup_project_handoff_delivery;

CREATE TABLE project_message_targets (
 device_id TEXT NOT NULL,
 client_message_id TEXT NOT NULL,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 model TEXT NOT NULL,
 effort TEXT,
 service_tier TEXT,
 created_at TEXT NOT NULL,
 PRIMARY KEY(device_id, client_message_id)
);

INSERT INTO project_message_targets SELECT * FROM cursor_backup_project_message_targets;

DROP TABLE cursor_backup_project_message_targets;

CREATE TABLE project_pending_handoff (
 conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 history_id INTEGER NOT NULL,
 delta_after_history_id INTEGER,
 created_at TEXT NOT NULL
);

INSERT INTO project_pending_handoff SELECT * FROM cursor_backup_project_pending_handoff;

DROP TABLE cursor_backup_project_pending_handoff;

CREATE TABLE provider_accounts (
 id TEXT PRIMARY KEY,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 label TEXT NOT NULL CHECK(length(trim(label)) BETWEEN 1 AND 60),
 provider_store TEXT NOT NULL UNIQUE,
 home_name TEXT NOT NULL UNIQUE CHECK(length(home_name) BETWEEN 1 AND 64),
 created_at TEXT NOT NULL
);

INSERT INTO provider_accounts SELECT * FROM cursor_backup_provider_accounts;

DROP TABLE cursor_backup_provider_accounts;

CREATE TABLE project_provider_accounts (
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 account_id TEXT NOT NULL REFERENCES provider_accounts(id) ON DELETE CASCADE,
 PRIMARY KEY(project_id, agent_family)
);

INSERT INTO project_provider_accounts SELECT * FROM cursor_backup_project_provider_accounts;

DROP TABLE cursor_backup_project_provider_accounts;

CREATE TABLE project_provider_refs (
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 provider_store TEXT NOT NULL,
 provider_project_id TEXT NOT NULL,
 PRIMARY KEY(project_id, agent_family, provider_store)
);

INSERT INTO project_provider_refs SELECT * FROM cursor_backup_project_provider_refs;

DROP TABLE cursor_backup_project_provider_refs;

CREATE TABLE project_roots (
 id TEXT PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 path TEXT NOT NULL,
 canonical_path TEXT NOT NULL,
 ordinal INTEGER NOT NULL,
 UNIQUE(project_id, canonical_path),
 UNIQUE(project_id, ordinal)
);

INSERT INTO project_roots SELECT * FROM cursor_backup_project_roots;

DROP TABLE cursor_backup_project_roots;

CREATE TABLE project_runtime_history (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 provider_store TEXT NOT NULL,
 native_session_id TEXT,
 runtime_thread_id TEXT,
 model TEXT,
 switched_at TEXT NOT NULL,
 -- How many readable history entries the thread had when it left, when known.
 last_position INTEGER CHECK(last_position IS NULL OR last_position >= 0)
);

INSERT INTO project_runtime_history SELECT * FROM cursor_backup_project_runtime_history;

DROP TABLE cursor_backup_project_runtime_history;

CREATE TABLE "provider_settings" (
  agent_family TEXT NOT NULL CHECK (agent_family IN ('codex','claude','opencode','cursor')),
  setting TEXT NOT NULL CHECK (setting IN ('file_checkpointing','desktop_computer_use')),
  enabled INTEGER NOT NULL CHECK (enabled = 1),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (agent_family, setting)
);

INSERT INTO provider_settings SELECT * FROM cursor_backup_provider_settings;

DROP TABLE cursor_backup_provider_settings;

CREATE TABLE runtime_bindings (
 conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode','cursor')),
 runtime_thread_id TEXT NOT NULL,
 session_id TEXT,
 execution_scope TEXT NOT NULL CHECK(execution_scope = 'projects'),
 provider_store TEXT NOT NULL,
 updated_at TEXT NOT NULL
);

INSERT INTO runtime_bindings SELECT * FROM cursor_backup_runtime_bindings;

DROP TABLE cursor_backup_runtime_bindings;

CREATE TABLE thread_delegations (
  child_conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  parent_conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  client_request_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  wake_message_id TEXT,
  observed_at TEXT, result_outcome TEXT, result_text TEXT, result_truncated INTEGER NOT NULL DEFAULT 0, result_at TEXT,
  UNIQUE(parent_conversation_id, client_request_id)
);

INSERT INTO thread_delegations SELECT * FROM cursor_backup_thread_delegations;

DROP TABLE cursor_backup_thread_delegations;

-- Preserve AUTOINCREMENT high-water marks even when the latest row was deleted.
UPDATE sqlite_sequence SET seq = max(seq, coalesce((SELECT seq FROM cursor_backup_sequence old WHERE old.name=sqlite_sequence.name), seq));
DROP TABLE cursor_backup_sequence;

CREATE UNIQUE INDEX claude_runtime_thread_owner ON runtime_bindings(runtime_thread_id) WHERE agent_family='claude';

CREATE INDEX runtime_binding_thread ON runtime_bindings(runtime_thread_id);

CREATE UNIQUE INDEX project_native_session_owner
 ON project_conversations(agent_family, provider_store, native_session_id)
 WHERE native_session_id IS NOT NULL;

CREATE INDEX project_conversation_recent ON project_conversations(project_id, is_pinned DESC, last_activity_at DESC);

CREATE UNIQUE INDEX project_root_owner ON project_roots(project_id, id);

CREATE INDEX thread_delegations_parent ON thread_delegations(parent_conversation_id);

CREATE INDEX project_runtime_history_conversation ON project_runtime_history(conversation_id, id);

CREATE TRIGGER runtime_binding_family_is_fixed BEFORE UPDATE OF agent_family ON runtime_bindings
WHEN NEW.agent_family != OLD.agent_family
BEGIN
 SELECT RAISE(ABORT,'A runtime session cannot move between agent families');
END;

CREATE TRIGGER project_model_stays_in_family BEFORE UPDATE OF model ON project_conversations
WHEN NEW.model IS NOT NULL AND NEW.agent_family !=
 CASE WHEN NEW.model LIKE 'claude:%' THEN 'claude'
      WHEN NEW.model LIKE 'opencode:%' THEN 'opencode'
      WHEN NEW.model LIKE 'cursor:%' THEN 'cursor' ELSE 'codex' END
BEGIN
 SELECT RAISE(ABORT,'Choose a model from this conversation agent family');
END;

CREATE TRIGGER runtime_binding_scope_is_fixed BEFORE UPDATE OF execution_scope ON runtime_bindings
WHEN NEW.execution_scope != OLD.execution_scope
BEGIN
 SELECT RAISE(ABORT,'A runtime session cannot move between execution scopes');
END;

CREATE TRIGGER project_conversation_identity_is_fixed BEFORE UPDATE OF
 project_id, agent_family, provider_store, cwd ON project_conversations
WHEN (NEW.project_id != OLD.project_id OR NEW.cwd != OLD.cwd
 OR ((NEW.agent_family != OLD.agent_family OR NEW.provider_store != OLD.provider_store)
  AND NOT EXISTS(SELECT 1 FROM project_runtime_switch_guard WHERE conversation_id=OLD.conversation_id)))
BEGIN
 SELECT RAISE(ABORT,'A project conversation keeps its provider identity and working folder');
END;

CREATE TRIGGER project_native_session_is_fixed BEFORE UPDATE OF native_session_id ON project_conversations
WHEN OLD.native_session_id IS NOT NULL AND NEW.native_session_id IS NOT OLD.native_session_id
 AND NOT EXISTS(SELECT 1 FROM project_runtime_switch_guard WHERE conversation_id=OLD.conversation_id)
BEGIN
 SELECT RAISE(ABORT,'A project conversation cannot move to another native session');
END;
