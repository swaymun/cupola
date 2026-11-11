CREATE TABLE notification_inbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    notification_json TEXT NOT NULL,
    started INTEGER NOT NULL DEFAULT 0
);
