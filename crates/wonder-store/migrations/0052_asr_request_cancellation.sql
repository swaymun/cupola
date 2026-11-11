-- A phone may cancel before it receives the accepted job id. Retain the request
-- identity so a delayed upload cannot start work after that cancellation.
CREATE TABLE asr_cancelled_requests (
    device_id TEXT NOT NULL,
    client_request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (device_id, client_request_id)
);
