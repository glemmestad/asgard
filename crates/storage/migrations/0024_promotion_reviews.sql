-- Machine-review verdicts for promotions: one row per reviewer per attempt.
-- verdict_json holds the full structured verdict, cost_usd attributes any model
-- spend the judge incurred. Audit + the advisory per-project attempt count.
-- (Keep semicolons out of comments — the migration splitter splits on them.)
CREATE TABLE IF NOT EXISTS promotion_reviews (
    id            TEXT PRIMARY KEY,
    request_id    TEXT NOT NULL,
    project_id    TEXT NOT NULL,
    reviewer_id   TEXT NOT NULL,
    kind          TEXT NOT NULL,
    disposition   TEXT NOT NULL,
    verdict_json  TEXT NOT NULL DEFAULT '{}',
    model         TEXT NOT NULL DEFAULT '',
    cost_usd      DOUBLE PRECISION NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_promrev_request ON promotion_reviews(request_id);
CREATE INDEX IF NOT EXISTS idx_promrev_project ON promotion_reviews(project_id);
