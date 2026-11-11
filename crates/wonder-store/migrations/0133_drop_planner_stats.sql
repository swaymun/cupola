-- Hosts briefly ran a sampled ANALYZE at open. Those statistics are never
-- refreshed and turn several indexed claim/recovery reads into scans; the
-- planner's defaults are what the queries are written and tested against.
DROP TABLE IF EXISTS sqlite_stat1;
