-- A thread settled, snoozed or archived in its home app (T3 Code, the Claude
-- app) that the owner restored in Cupola only: `marker` names that closing,
-- so the thread shows again until the home app closes it anew.
CREATE TABLE kept_open_elsewhere (
 conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
 marker TEXT NOT NULL
);
