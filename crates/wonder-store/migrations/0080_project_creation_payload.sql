-- Keep first-send settings independent of later conversation PATCHes. Older
-- rows can only be backfilled from their settings at migration time; their
-- original pre-edit request cannot be reconstructed.
ALTER TABLE project_conversations ADD COLUMN creation_payload TEXT;
UPDATE project_conversations SET creation_payload = json_array(
 project_id, agent_family, provider_store, cwd, roots_revision, model, effort,
 access_mode, claude_approval, plan_mode
) WHERE creation_request_id IS NOT NULL;
