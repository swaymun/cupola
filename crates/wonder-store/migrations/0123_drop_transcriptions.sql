-- Voice transcription was never shipped; its tables held no rows anyone reads.
DROP TABLE IF EXISTS asr_cancelled_requests;
DROP INDEX IF EXISTS transcription_request_identity;
DROP TABLE IF EXISTS transcriptions;
