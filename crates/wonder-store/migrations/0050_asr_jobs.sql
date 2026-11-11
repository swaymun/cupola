ALTER TABLE transcriptions ADD COLUMN client_request_id TEXT;
ALTER TABLE transcriptions ADD COLUMN request_sha256 TEXT;
ALTER TABLE transcriptions ADD COLUMN model_id TEXT NOT NULL DEFAULT 'parakeet-tdt-0.6b-v3-q8';
ALTER TABLE transcriptions ADD COLUMN language TEXT NOT NULL DEFAULT 'auto';
ALTER TABLE transcriptions ADD COLUMN audio_bytes BLOB;
ALTER TABLE transcriptions ADD COLUMN audio_mime TEXT;
CREATE UNIQUE INDEX transcription_request_identity ON transcriptions(source_device_id, client_request_id);
