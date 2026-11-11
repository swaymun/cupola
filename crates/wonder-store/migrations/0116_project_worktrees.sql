-- One host-wide Project discovery choice. A missing row means on.
CREATE TABLE project_settings (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    include_worktrees INTEGER NOT NULL CHECK (include_worktrees IN (0, 1))
);
