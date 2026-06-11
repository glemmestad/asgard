-- 0007: recipes — parameterized provisioning compositions. A recipe names a set
-- of catalog primitives and how to wire their inputs/outputs together. The agent
-- fills the params and issues each request_resource call itself (Frontkeep stays a
-- hub, not an orchestrator). spec is JSON with description, inputs, steps, outputs.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
CREATE TABLE IF NOT EXISTS recipes (
    slug       TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    summary    TEXT NOT NULL DEFAULT '',
    spec       TEXT NOT NULL DEFAULT '{}',
    tags       TEXT NOT NULL DEFAULT '',
    author     TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);
