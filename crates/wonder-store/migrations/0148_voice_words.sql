-- The owner's voice dictionary (Oct 29): words and names voice mode should
-- transcribe correctly, such as "T3" or "Cupola". Sent to the transcriber when
-- a voice session starts. Case-insensitive unique; oldest first.
CREATE TABLE voice_words (
  word TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
  added_at_ms INTEGER NOT NULL
);
