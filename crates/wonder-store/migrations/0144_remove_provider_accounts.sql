-- Added sign-ins are no longer supported. Keep conversation/runtime store
-- identities untouched: history from those homes must remain read-only, never
-- resume with the computer's sign-in. This migration performs no filesystem IO.
-- Provider/model choices live in projects/project_last_models and survive.
DROP TABLE project_provider_accounts;
DROP TABLE provider_accounts;
