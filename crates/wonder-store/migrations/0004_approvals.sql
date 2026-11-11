CREATE TABLE IF NOT EXISTS approvals (
    approval_id TEXT PRIMARY KEY NOT NULL,
    server_request_id TEXT NOT NULL UNIQUE,
    method TEXT NOT NULL,
    params_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'resolving', 'resolved')),
    decision TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);
