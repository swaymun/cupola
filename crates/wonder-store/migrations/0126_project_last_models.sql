-- New threads start from the chat's own choice, then the model last chosen
-- for that provider in the Project, then Settings → Default models (owner,
-- Oct 21). The host keeps the Project's last choice, so every device agrees;
-- it replaces the explicit per-Project thread defaults.
CREATE TABLE project_last_models (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  agent_family TEXT NOT NULL,
  model TEXT NOT NULL,
  effort TEXT,
  PRIMARY KEY (project_id, agent_family)
);
INSERT INTO project_last_models(project_id, agent_family, model, effort)
  SELECT project_id, agent_family, model, effort FROM project_thread_defaults WHERE model IS NOT NULL;
DROP TABLE project_thread_defaults;
