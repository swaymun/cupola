ALTER TABLE pending_app_server_notifications
    ADD COLUMN notification_json TEXT NOT NULL DEFAULT '{}';
