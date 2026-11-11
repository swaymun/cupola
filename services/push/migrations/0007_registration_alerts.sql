-- Banners are a separate choice from Live Activities. A registration whose
-- phone turned Notifications off keeps receiving Live Activity updates but gets
-- no alert push. Existing registrations keep alerting until the phone says otherwise.
ALTER TABLE registrations ADD COLUMN alerts INTEGER NOT NULL DEFAULT 1;
