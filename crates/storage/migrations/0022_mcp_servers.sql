-- MCP catalog: user-publishable MCP servers, separate from the project/service
-- provisioning catalog. status is the trust tier (community vs company-approved);
-- state is the lifecycle (active/disabled/archived) for hiding and pruning.
CREATE TABLE IF NOT EXISTS mcp_servers (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    summary     TEXT NOT NULL DEFAULT '',
    readme      TEXT NOT NULL DEFAULT '',
    install     TEXT NOT NULL DEFAULT '{}',
    repository  TEXT NOT NULL DEFAULT '',
    homepage    TEXT NOT NULL DEFAULT '',
    version     TEXT NOT NULL DEFAULT '',
    tags        TEXT NOT NULL DEFAULT '',
    owner       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'community',
    state       TEXT NOT NULL DEFAULT 'active',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    approved_at TEXT,
    approved_by TEXT
);
