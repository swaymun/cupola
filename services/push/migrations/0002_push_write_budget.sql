-- Keep the same receipt columns and uniqueness for existing clients/Worker code.
-- The composite primary key is the table: no duplicate rowid/unique index writes.
CREATE TABLE deliveries_compact (
 registration_id TEXT NOT NULL,
 event_id TEXT NOT NULL,
 route_id TEXT NOT NULL,
 kind TEXT NOT NULL,
 state TEXT NOT NULL,
 attempts INTEGER NOT NULL DEFAULT 0,
 updated INTEGER NOT NULL,
 PRIMARY KEY(registration_id,event_id)
) WITHOUT ROWID;
INSERT INTO deliveries_compact SELECT * FROM deliveries;
DROP TABLE deliveries;
ALTER TABLE deliveries_compact RENAME TO deliveries;

CREATE TABLE limits_compact (
 key TEXT PRIMARY KEY,
 count INTEGER NOT NULL,
 expires INTEGER NOT NULL
) WITHOUT ROWID;
-- Old keys ended in :<time bucket>. SQLite's bare count with MAX(expires)
-- selects the newest bucket's count, preserving current quotas at migration.
INSERT INTO limits_compact
 SELECT substr(rtrim(key,'0123456789'),1,length(rtrim(key,'0123456789'))-1),
        count,MAX(expires)
 FROM limits WHERE key NOT LIKE 'ip:%'
 GROUP BY substr(rtrim(key,'0123456789'),1,length(rtrim(key,'0123456789'))-1);
DROP TABLE limits;
ALTER TABLE limits_compact RENAME TO limits;
