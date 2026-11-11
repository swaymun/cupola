-- Additional provider accounts. Each is a separate provider home (for Codex,
-- its own CODEX_HOME) that Wonder created under its data directory; the Mac's
-- own home is the implicit default and is never recorded here. A Project
-- thread belongs to the account whose home holds its history: its
-- project_conversations.provider_store equals the account's provider_store.
CREATE TABLE provider_accounts (
 id TEXT PRIMARY KEY,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 label TEXT NOT NULL CHECK(length(trim(label)) BETWEEN 1 AND 60),
 provider_store TEXT NOT NULL UNIQUE,
 home_name TEXT NOT NULL UNIQUE CHECK(length(home_name) BETWEEN 1 AND 64),
 created_at TEXT NOT NULL
);

-- The account a Project's new threads use when the phone does not choose one.
CREATE TABLE project_provider_accounts (
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
 agent_family TEXT NOT NULL CHECK(agent_family IN ('codex','claude','opencode')),
 account_id TEXT NOT NULL REFERENCES provider_accounts(id) ON DELETE CASCADE,
 PRIMARY KEY(project_id, agent_family)
);
