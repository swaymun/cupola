-- Outside agents are OAuth clients, never owner devices or phone sessions.
CREATE TABLE mcp_clients (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  redirect_uris TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  revoked_at_ms INTEGER
);
CREATE TABLE mcp_pairing_codes (
  code_hash TEXT PRIMARY KEY NOT NULL,
  expires_at_ms INTEGER NOT NULL
);
CREATE TABLE mcp_authorization_codes (
  code_hash TEXT PRIMARY KEY NOT NULL,
  client_id TEXT NOT NULL REFERENCES mcp_clients(id),
  redirect_uri TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  audience TEXT NOT NULL,
  scopes TEXT NOT NULL,
  permission_mode TEXT NOT NULL CHECK(permission_mode IN ('read_only','workspace','full_access')),
  expires_at_ms INTEGER NOT NULL
);
CREATE TABLE mcp_grants (
  id TEXT PRIMARY KEY NOT NULL,
  client_id TEXT NOT NULL REFERENCES mcp_clients(id),
  audience TEXT NOT NULL,
  scopes TEXT NOT NULL,
  permission_mode TEXT NOT NULL CHECK(permission_mode IN ('read_only','workspace','full_access')),
  expires_at_ms INTEGER NOT NULL,
  revoked_at_ms INTEGER
);
CREATE INDEX mcp_grants_client ON mcp_grants(client_id);
CREATE TABLE mcp_tokens (
  token_hash TEXT PRIMARY KEY NOT NULL,
  grant_id TEXT NOT NULL REFERENCES mcp_grants(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK(kind IN ('access','refresh')),
  expires_at_ms INTEGER NOT NULL,
  used_at_ms INTEGER
);
CREATE INDEX mcp_tokens_grant ON mcp_tokens(grant_id);
