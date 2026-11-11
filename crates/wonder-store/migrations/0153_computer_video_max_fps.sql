-- The frame-rate ceiling the viewer asked for. Sessions opened before the
-- field existed, and clients that do not send it, keep the original 15.
ALTER TABLE computer_sessions ADD COLUMN video_max_fps INTEGER NOT NULL DEFAULT 15
    CHECK(video_max_fps BETWEEN 15 AND 120);
