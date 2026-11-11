-- Turns run outside Wonder (Claude Code or the Codex app on this Mac) in a
-- thread Wonder follows have no Wonder event to read a preview from; the
-- watcher keeps the reply excerpt with the intent. Local to this Mac, like
-- the event history.
ALTER TABLE push_intents ADD COLUMN preview_body TEXT;
