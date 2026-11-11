-- Typed native Codex inputs share the message's durable acceptance boundary.
ALTER TABLE messages ADD COLUMN codex_skills_json TEXT NOT NULL DEFAULT '[]';
-- Receipt identity stays fixed when a queued edit removes selected tokens.
ALTER TABLE messages ADD COLUMN original_codex_skills_json TEXT NOT NULL DEFAULT '[]';
