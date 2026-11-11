-- Replay retention drops receipts past the replay age with an index range
-- instead of scanning every receipt each maintenance pass.
CREATE INDEX IF NOT EXISTS app_server_notification_receipts_received
    ON app_server_notification_receipts(received_at);
