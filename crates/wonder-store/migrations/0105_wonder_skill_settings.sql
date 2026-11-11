-- The owner's switch for each skill in Wonder's own skill pack. A missing row
-- means the skill is off, which is the default.
CREATE TABLE wonder_skill_settings (
  skill TEXT PRIMARY KEY NOT NULL,
  enabled INTEGER NOT NULL CHECK (enabled = 1),
  updated_at TEXT NOT NULL
);
