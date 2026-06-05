-- Async code-review jobs: one row per promotion that needs a background review.
-- The worker leases a pending row (status running + lease_until), runs the panel,
-- finalizes the promotion, then marks it done. A crashed run leaves a stale lease
-- that the next sweep reclaims, so review survives a restart (state lives here, not
-- in memory). attempts bounds retries before failing closed.
-- (Keep semicolons out of comments — the migration splitter splits on them.)
CREATE TABLE IF NOT EXISTS review_jobs (
    id            TEXT PRIMARY KEY,
    request_id    TEXT NOT NULL,
    project_id    TEXT NOT NULL,
    target        TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending',
    attempts      INTEGER NOT NULL DEFAULT 0,
    lease_until   TEXT,
    error         TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reviewjobs_status ON review_jobs(status);
CREATE INDEX IF NOT EXISTS idx_reviewjobs_request ON review_jobs(request_id);
