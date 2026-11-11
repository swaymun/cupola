-- A project is a named group of source folders on this Mac. The store belongs
-- to one host installation, so a project can never reference another Mac.
CREATE TABLE projects (
 id TEXT PRIMARY KEY,
 name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 120),
 is_included INTEGER NOT NULL DEFAULT 1 CHECK(is_included IN (0,1)),
 pin_order INTEGER,
 primary_root_id TEXT NOT NULL,
 roots_revision INTEGER NOT NULL DEFAULT 1 CHECK(roots_revision >= 1),
 last_family TEXT CHECK(last_family IN ('codex','claude','opencode')),
 creation_request_id TEXT UNIQUE,
 creation_payload_sha256 TEXT,
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 last_used_at TEXT
);

CREATE TABLE project_roots (
 id TEXT PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 path TEXT NOT NULL,
 canonical_path TEXT NOT NULL,
 ordinal INTEGER NOT NULL,
 UNIQUE(project_id, canonical_path),
 UNIQUE(project_id, ordinal)
);

-- Native provider identity for a Wonder project, e.g. a Codex project ID.
-- provider_store names private host configuration; it is never sent to clients.
CREATE TABLE project_provider_refs (
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 provider_store TEXT NOT NULL,
 provider_project_id TEXT NOT NULL,
 PRIMARY KEY(project_id, agent_family, provider_store)
);

-- Project conversations own their provider identity and settings.
CREATE TABLE project_conversations (
 conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
 project_id TEXT NOT NULL REFERENCES projects(id),
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
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
 last_activity_at TEXT NOT NULL,
 CHECK(agent_family != 'opencode' OR access_mode = 'workspace')
);
CREATE UNIQUE INDEX project_native_session_owner
 ON project_conversations(agent_family, provider_store, native_session_id)
 WHERE native_session_id IS NOT NULL;
CREATE INDEX project_conversation_recent ON project_conversations(project_id, is_pinned DESC, last_activity_at DESC);

CREATE TRIGGER project_conversation_identity_is_fixed BEFORE UPDATE OF
 project_id, agent_family, provider_store, cwd ON project_conversations
WHEN NEW.project_id != OLD.project_id OR NEW.agent_family != OLD.agent_family
 OR NEW.provider_store != OLD.provider_store OR NEW.cwd != OLD.cwd
BEGIN
 SELECT RAISE(ABORT,'A project conversation keeps its provider identity and working folder');
END;
CREATE TRIGGER project_native_session_is_fixed BEFORE UPDATE OF native_session_id ON project_conversations
WHEN OLD.native_session_id IS NOT NULL AND NEW.native_session_id IS NOT OLD.native_session_id
BEGIN
 SELECT RAISE(ABORT,'A project conversation cannot move to another native session');
END;
CREATE TRIGGER project_model_stays_in_family BEFORE UPDATE OF model ON project_conversations
WHEN NEW.model IS NOT NULL AND NEW.agent_family !=
 CASE WHEN NEW.model LIKE 'claude:%' THEN 'claude'
      WHEN NEW.model LIKE 'opencode:%' THEN 'opencode' ELSE 'codex' END
BEGIN
 SELECT RAISE(ABORT,'Choose a model from this conversation agent family');
END;

CREATE TRIGGER runtime_binding_scope_is_fixed BEFORE UPDATE OF execution_scope ON runtime_bindings
WHEN NEW.execution_scope != OLD.execution_scope
BEGIN
 SELECT RAISE(ABORT,'A runtime session cannot move between execution scopes');
END;
