-- 0019: cross-instance coordination leases. One row per named lease, used to
-- make horizontal scale-out safe: per-tick election of the background loops and
-- a per-resource lock around Terraform applies. holder is a per-process id and
-- expires_at is an RFC3339 UTC instant in the same format as now(), so it is
-- compared lexicographically like sessions.expires_at and review_date elsewhere.
-- A lease is free once expires_at is in the past. Portable SQL only.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
CREATE TABLE IF NOT EXISTS leases (
  name       TEXT PRIMARY KEY,
  holder     TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
