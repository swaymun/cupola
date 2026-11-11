-- Models the owner chose not to offer for a harness. They leave the composer,
-- New chat and the agent tools' model lists; a missing row means offered.
CREATE TABLE hidden_models (
  agent_family TEXT NOT NULL,
  model_id TEXT NOT NULL,
  PRIMARY KEY (agent_family, model_id)
);
