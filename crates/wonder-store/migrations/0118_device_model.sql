-- The hardware model a phone reports when it pairs ("iPhone17,2"), so the
-- device list can tell phones apart when their names are all "iPhone".
ALTER TABLE devices ADD COLUMN model TEXT;
