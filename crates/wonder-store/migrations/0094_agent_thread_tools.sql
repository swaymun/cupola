-- Agents in a Project can message other threads of the same Project and
-- delegate work to new ones. A message sent that way remembers which thread
-- wrote it, so the timeline can say so and a wake-up can stay out of the queue.
CREATE TABLE message_sources (
  message_id TEXT PRIMARY KEY NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK(kind IN ('thread','delegation','wake')),
  -- Null for a wake-up, which comes from Wonder rather than from one thread.
  source_conversation_id TEXT,
  source_title TEXT NOT NULL
);

-- A thread an agent created for a bounded task. wake_message_id is set when a
-- wake-up told the parent that this child finished; observed_at when the
-- parent read the result itself. Either clears the pending wake.
CREATE TABLE thread_delegations (
  child_conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  parent_conversation_id TEXT NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  client_request_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  wake_message_id TEXT,
  observed_at TEXT,
  UNIQUE(parent_conversation_id, client_request_id)
);
CREATE INDEX thread_delegations_parent ON thread_delegations(parent_conversation_id);
