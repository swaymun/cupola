-- A chat may override its provider's chat detail; NULL follows the provider.
ALTER TABLE project_conversations ADD COLUMN detail_override TEXT
    CHECK(detail_override IN ('everything','conversation','thinking'));
