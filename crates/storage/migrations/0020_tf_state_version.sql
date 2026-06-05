-- 0020: optimistic-concurrency version on tf_state. The per-resource TF lease
-- keeps two applies from running at once, but a lease over a non-cooperative
-- external resource cannot prevent a paused holder from persisting stale state
-- after another instance advanced it. This column is the backstop: persist is a
-- compare-and-swap on version, so a stale write fails loudly instead of silently
-- clobbering. Existing rows start at 0 and the next apply's CAS bumps them.
-- ALTER ADD COLUMN with a constant default is portable across SQLite and Postgres.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
ALTER TABLE tf_state ADD COLUMN version INTEGER NOT NULL DEFAULT 0;
