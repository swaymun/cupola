-- Relay resources outlive account/session rows until external deletion succeeds.
-- No FK cascade may erase the only record needed to retry Cloudflare cleanup.
-- Enabled hosts belong to the account; only unredeemed grants bind a session.
CREATE TABLE relay_hosts (
 id TEXT PRIMARY KEY,
 account_id TEXT NOT NULL,
 label TEXT NOT NULL UNIQUE,
 host_key TEXT NOT NULL UNIQUE,
 tunnel_id TEXT,
 dns_id TEXT,
 state TEXT NOT NULL CHECK(state IN ('granted','enabled','deleting')),
 last_seen INTEGER NOT NULL,
 created INTEGER NOT NULL
);
CREATE INDEX relay_hosts_account ON relay_hosts(account_id);
CREATE TABLE relay_grants (
 hash TEXT PRIMARY KEY,
 label TEXT NOT NULL,
 session_id TEXT NOT NULL,
 expires INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX relay_grants_label ON relay_grants(label);
CREATE TABLE relay_pending (
 id TEXT PRIMARY KEY,
 host_key TEXT NOT NULL,
 secret_hash TEXT NOT NULL,
 label TEXT,
 account_id TEXT,
 session_id TEXT,
 expires INTEGER NOT NULL
);
CREATE TABLE relay_nonces (hash TEXT PRIMARY KEY, expires INTEGER NOT NULL) WITHOUT ROWID;
-- One short global control-plane lease serializes tunnel creation and reaping
-- across isolates. A crashed operation is recovered by its deterministic name.
CREATE TABLE relay_locks (id TEXT PRIMARY KEY, owner TEXT NOT NULL, expires INTEGER NOT NULL) WITHOUT ROWID;
