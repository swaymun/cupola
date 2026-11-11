-- Only visibility changes invalidate the Live Activity projection. Message,
-- streaming, device-presence and read-state writes must not refresh folders.
CREATE TABLE project_visibility_revision (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL);
INSERT INTO project_visibility_revision VALUES(1,0);
CREATE TRIGGER visibility_projects_insert AFTER INSERT ON projects BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_projects_delete AFTER DELETE ON projects BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_projects_update AFTER UPDATE OF is_included ON projects WHEN OLD.is_included IS NOT NEW.is_included BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_roots_insert AFTER INSERT ON project_roots BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_roots_delete AFTER DELETE ON project_roots BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_roots_update AFTER UPDATE OF project_id,canonical_path ON project_roots WHEN OLD.project_id IS NOT NEW.project_id OR OLD.canonical_path IS NOT NEW.canonical_path BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_conversations_insert AFTER INSERT ON project_conversations BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_conversations_delete AFTER DELETE ON project_conversations BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_conversations_update AFTER UPDATE OF project_id,cwd,wonder_archived_at ON project_conversations WHEN OLD.project_id IS NOT NEW.project_id OR OLD.cwd IS NOT NEW.cwd OR OLD.wonder_archived_at IS NOT NEW.wonder_archived_at BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_settings_insert AFTER INSERT ON project_settings BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_settings_delete AFTER DELETE ON project_settings BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
CREATE TRIGGER visibility_project_settings_update AFTER UPDATE OF include_worktrees ON project_settings WHEN OLD.include_worktrees IS NOT NEW.include_worktrees BEGIN UPDATE project_visibility_revision SET revision=revision+1 WHERE id=1; END;
