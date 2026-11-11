-- How a Claude project thread asks before acting, and whether the thread is in
-- plan mode. access_mode keeps its meaning; claude_approval only applies when
-- access_mode is workspace and the thread is a Claude thread.
ALTER TABLE project_conversations ADD COLUMN claude_approval TEXT NOT NULL DEFAULT 'ask'
 CHECK(claude_approval IN ('ask','accept_edits','auto'));
ALTER TABLE project_conversations ADD COLUMN plan_mode INTEGER NOT NULL DEFAULT 0
 CHECK(plan_mode IN (0,1));
