ALTER TABLE approvals ADD COLUMN resolution_idempotency_key TEXT;
ALTER TABLE approvals ADD COLUMN resolution_body_sha256 TEXT;
ALTER TABLE approvals ADD COLUMN resolution_json TEXT;
