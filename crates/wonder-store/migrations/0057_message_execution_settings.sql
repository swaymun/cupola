CREATE TABLE message_execution_settings (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    model TEXT, reasoning_effort TEXT, service_tier TEXT,
    working_directory TEXT
);
