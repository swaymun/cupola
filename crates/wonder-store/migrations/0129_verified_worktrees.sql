-- Linked Git worktrees of a Project folder, as wonderd last verified them
-- from Git's own files (projects::registered_worktree). A thread working in
-- one belongs to every Project that saves `root_path` while worktree chats
-- are on (verified_subagent_root_sql). Rebuilt from disk on discovery; it is
-- never the only record of anything. Rows for a folder no Project saves
-- grant nothing (membership joins through project_roots); they are kept so a
-- folder saved again is owned at once, and rediscovery replaces them.
CREATE TABLE project_worktrees (
    root_path TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (root_path, path)
);
CREATE INDEX project_worktrees_path ON project_worktrees(path);
