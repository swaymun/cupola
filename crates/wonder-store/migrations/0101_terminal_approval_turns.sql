-- A terminal notification may precede both its receipt and a delayed request.
-- Keep exact transport identity; message/history rows have no runtime identity.
CREATE TABLE terminal_approval_turns (
    runtime_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    PRIMARY KEY (runtime_id, thread_id, turn_id)
);
CREATE INDEX approvals_pending_turn ON approvals (
    thread_id, turn_id, json_extract(params_json, '$._wonderRuntimeId')
) WHERE state IN ('pending', 'resolving');
