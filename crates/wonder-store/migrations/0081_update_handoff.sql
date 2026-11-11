-- Only an explicit update handoff authorizes an automatic continuation.
CREATE TABLE update_turn_settings (
    thread_id TEXT PRIMARY KEY,
    resume_params TEXT NOT NULL,
    turn_params TEXT NOT NULL
);
CREATE TABLE update_handoffs (
    thread_id TEXT NOT NULL,
    stopped_turn_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    message_id TEXT REFERENCES messages(id) ON DELETE CASCADE,
    resume_client_id TEXT NOT NULL UNIQUE,
    resume_params TEXT NOT NULL,
    turn_params TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('paused','submitting','resumed','cancelled','finished')),
    resumed_turn_id TEXT,
    reported_error TEXT,
    PRIMARY KEY(thread_id, stopped_turn_id)
);
