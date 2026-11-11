-- Per-harness runtime settings the owner chose on this computer: binary path,
-- launch arguments, environment variable names and non-secret values, Codex
-- home and Claude's idle compaction. `config` is JSON; secret values never
-- live here (they stay in the login Keychain, only their names are saved).
CREATE TABLE provider_runtime_config (
  agent_family TEXT PRIMARY KEY,
  config TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
