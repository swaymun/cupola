-- Remote review is an explicit grant, separate from local folder access.
-- The database is host-local; credentials are never stored in this grant.
ALTER TABLE projects ADD COLUMN github_review_revision INTEGER NOT NULL DEFAULT 0
 CHECK(github_review_revision >= 0);
CREATE UNIQUE INDEX project_root_owner ON project_roots(project_id, id);
CREATE TABLE project_github_review_bindings (
 project_id TEXT NOT NULL,
 root_id TEXT NOT NULL,
 roots_revision INTEGER NOT NULL CHECK(roots_revision >= 1),
 account_id INTEGER NOT NULL CHECK(account_id > 0),
 repository_id INTEGER NOT NULL CHECK(repository_id > 0),
 repository TEXT NOT NULL,
 authorized_at TEXT NOT NULL,
 PRIMARY KEY(project_id, root_id),
 FOREIGN KEY(project_id, root_id) REFERENCES project_roots(project_id, id) ON DELETE CASCADE
);

-- Exclusion or any folder revision requires a new explicit authorization,
-- even if the owner later restores the same folder selection.
CREATE TRIGGER revoke_project_github_review AFTER UPDATE OF is_included, roots_revision ON projects
WHEN NEW.is_included = 0 OR NEW.roots_revision != OLD.roots_revision
BEGIN
 DELETE FROM project_github_review_bindings WHERE project_id = NEW.id;
 UPDATE projects SET github_review_revision=github_review_revision+1 WHERE id=NEW.id;
END;
