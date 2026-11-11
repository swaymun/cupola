-- A chosen title (the owner's rename, or one Cupola gave: a fork's, a child
-- task's name) is shown as it is. A title Cupola took from the thread's first
-- message or a provider's name follows the list title rule instead.
-- Rows stored before this column kept no such record, so every title they
-- hold counts as chosen and is never rewritten. A new row's title counts as
-- chosen until its creator marks it taken.
ALTER TABLE project_conversations ADD COLUMN title_chosen INTEGER NOT NULL DEFAULT 1
  CHECK (title_chosen IN (0, 1));
