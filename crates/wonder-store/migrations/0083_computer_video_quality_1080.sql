-- Keep the original column for existing sessions and older app versions. An
-- additive column avoids rebuilding a table referenced by control leases.
ALTER TABLE computer_sessions ADD COLUMN video_quality_v2 TEXT
    CHECK(video_quality_v2 IS NULL OR video_quality_v2 IN ('standard', 'medium', 'auto', 'high'));
