-- Where an agent's picture already lives on this Mac, so Cupola keeps a
-- reference instead of a second full copy under project-media. A
-- conversation_files row with a source has no relative_path; its byte_size
-- and sha256 are the original's, and every read is verified against them.
-- `file`: the image file the tool showed (Codex imageView). `transcript`:
-- base64 text inside a provider transcript (Claude JSONL) at
-- [offset, offset + length). Paths never leave the Mac. Existing copied
-- previews keep their copies.
CREATE TABLE conversation_file_sources (
    file_id TEXT PRIMARY KEY NOT NULL
        REFERENCES conversation_files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'transcript')),
    path TEXT NOT NULL,
    offset INTEGER CHECK (offset IS NULL OR offset >= 0),
    length INTEGER CHECK (length IS NULL OR length > 0),
    width INTEGER,
    height INTEGER,
    created_at TEXT NOT NULL,
    CHECK ((kind = 'file') = (offset IS NULL AND length IS NULL))
);
