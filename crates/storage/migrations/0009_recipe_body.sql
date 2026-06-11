-- 0009: recipes carry a rich markdown runbook (body), not just a structured
-- step list. The body is the narrated agent guide — what you build, the image and
-- env contract, the ordered Frontkeep calls, how to verify, gotchas. The structured
-- spec stays as an at-a-glance supplement.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
ALTER TABLE recipes ADD COLUMN body TEXT NOT NULL DEFAULT '';
