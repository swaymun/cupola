-- An Automation edit and its scheduled claim advance the same durable
-- revision so an editor cannot silently overwrite a newer change.
ALTER TABLE automations ADD COLUMN revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0);
