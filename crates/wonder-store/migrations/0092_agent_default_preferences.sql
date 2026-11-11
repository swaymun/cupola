-- The owner's default model, effort and speed per agent harness. A missing row
-- means Wonder's own rule applies.
CREATE TABLE agent_default_preferences (
  agent_family TEXT PRIMARY KEY NOT NULL,
  model TEXT,
  effort TEXT,
  service_tier TEXT,
  updated_at TEXT NOT NULL
);
