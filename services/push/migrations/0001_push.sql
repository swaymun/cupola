-- No accounts or conversation content. Challenges expire after ten minutes.
CREATE TABLE challenges (id TEXT PRIMARY KEY, token TEXT NOT NULL, token_hash TEXT NOT NULL, public_key TEXT NOT NULL, nonce TEXT NOT NULL, challenge_hash TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE INDEX challenges_expiry ON challenges(expires);
CREATE TABLE registrations (id TEXT PRIMARY KEY, token TEXT NOT NULL, token_hash TEXT NOT NULL, public_key TEXT NOT NULL, sender_hash TEXT NOT NULL, updated INTEGER NOT NULL, UNIQUE(token_hash,public_key));
CREATE TABLE deliveries (registration_id TEXT NOT NULL, event_id TEXT NOT NULL, route_id TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL, PRIMARY KEY(registration_id,event_id));
CREATE INDEX deliveries_expiry ON deliveries(updated);
CREATE TABLE limits (key TEXT PRIMARY KEY, count INTEGER NOT NULL, expires INTEGER NOT NULL);
