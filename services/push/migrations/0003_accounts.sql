-- Accounts own push. An account is an ID plus linked sign-in identities (only
-- Sign in with Apple today); subjects are peppered HMACs, never plaintext.
-- Pre-launch: existing challenges and registrations are dropped, not migrated;
-- phones re-enroll after signing in.
CREATE TABLE accounts (id TEXT PRIMARY KEY, created INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE identities (
 provider TEXT NOT NULL,
 subject_hash TEXT NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 created INTEGER NOT NULL,
 PRIMARY KEY(provider,subject_hash)
) WITHOUT ROWID;
CREATE INDEX identities_account ON identities(account_id);
-- One session per device key. Access and refresh tokens are stored as hashes.
CREATE TABLE sessions (
 id TEXT PRIMARY KEY,
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 device_key TEXT NOT NULL UNIQUE,
 access_hash TEXT NOT NULL UNIQUE,
 access_expires INTEGER NOT NULL,
 refresh_hash TEXT NOT NULL,
 refresh_expires INTEGER NOT NULL,
 created INTEGER NOT NULL
);
CREATE INDEX sessions_account ON sessions(account_id);
CREATE INDEX sessions_refresh_expiry ON sessions(refresh_expires);

DROP TABLE challenges;
CREATE TABLE challenges (
 id TEXT PRIMARY KEY, token TEXT NOT NULL, token_hash TEXT NOT NULL, public_key TEXT NOT NULL,
 nonce TEXT NOT NULL, challenge_hash TEXT NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
 expires INTEGER NOT NULL
);
CREATE INDEX challenges_expiry ON challenges(expires);
CREATE INDEX challenges_session ON challenges(session_id);

DELETE FROM deliveries;
DROP TABLE registrations;
CREATE TABLE registrations (
 id TEXT PRIMARY KEY, token TEXT NOT NULL, token_hash TEXT NOT NULL, public_key TEXT NOT NULL,
 sender_hash TEXT NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
 updated INTEGER NOT NULL,
 UNIQUE(token_hash,public_key)
);
CREATE INDEX registrations_session ON registrations(session_id);
CREATE INDEX registrations_account ON registrations(account_id);
