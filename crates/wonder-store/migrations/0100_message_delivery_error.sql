-- Why the Mac could not deliver a message (the agent's runtime refused it,
-- an attachment could not be read…), in the words shown to the owner and
-- to an agent that sent it. Cleared when the message is sent again.
ALTER TABLE messages ADD COLUMN delivery_error TEXT;
