ALTER TABLE computer_sessions ADD COLUMN video_quality TEXT NOT NULL DEFAULT 'standard'
    CHECK(video_quality IN ('standard', 'auto', 'high'));
