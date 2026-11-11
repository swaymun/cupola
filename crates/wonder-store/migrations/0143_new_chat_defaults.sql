-- This preference belongs to this computer, independently of provider models.
CREATE TABLE new_chat_defaults (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    starts_in_t3_code INTEGER NOT NULL DEFAULT 0 CHECK (starts_in_t3_code IN (0, 1))
);
