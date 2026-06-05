-- 0021: make cost-anomaly recording idempotent per (project, day, service) so a
-- rollup that re-runs — a retried tick, or a brief leader overlap during failover
-- — updates the day's anomaly in place instead of appending a duplicate row. The
-- rollup and forecast upserts are already idempotent, so this closes the last gap.
-- A DB upgraded from before this change may already hold duplicates for that key
-- (the old INSERT used a fresh id every run), which would make the unique index
-- fail to build, so collapse each group to its highest id first. Portable SQL.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
DELETE FROM cost_anomaly WHERE id NOT IN (SELECT MAX(id) FROM cost_anomaly GROUP BY project_id, day, service);
CREATE UNIQUE INDEX IF NOT EXISTS idx_anomaly_key ON cost_anomaly(project_id, day, service);
