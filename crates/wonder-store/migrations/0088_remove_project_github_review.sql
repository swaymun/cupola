-- GitHub pull-request review was withdrawn. Remove its saved repository grants;
-- local Project folders and their revisions are unchanged.
DROP TRIGGER IF EXISTS revoke_project_github_review;
DROP TABLE IF EXISTS project_github_review_bindings;
ALTER TABLE projects DROP COLUMN github_review_revision;
