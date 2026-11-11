ALTER TABLE conversation_files ADD COLUMN mime_type TEXT;
ALTER TABLE conversation_files ADD COLUMN byte_size INTEGER;
ALTER TABLE conversation_files ADD COLUMN sha256 TEXT;
