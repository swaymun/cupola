CREATE TABLE IF NOT EXISTS app_server_notification_receipts (
    notification_key TEXT PRIMARY KEY NOT NULL,
    received_at TEXT NOT NULL
);
