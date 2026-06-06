-- Skills catalog: user-publishable agent skills (a SKILL.md plus optional bundled
-- scripts/config/references), separate from the MCP and provisioning catalogs.
-- status is the trust tier (community vs company-approved). state is the lifecycle
-- of active/disabled/archived. runtime is the authored runtime, claude-code or codex.
-- The file tree is stored as canonical JSON in bundle (already 7-bit since file
-- bytes are base64'd inside), with bundle_sha256/bundle_bytes tracking it. manifest
-- is the parsed SKILL.md frontmatter. review_json holds the latest code-review assist.
CREATE TABLE IF NOT EXISTS skills (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    summary       TEXT NOT NULL DEFAULT '',
    readme        TEXT NOT NULL DEFAULT '',
    runtime       TEXT NOT NULL DEFAULT 'claude-code',
    manifest      TEXT NOT NULL DEFAULT '{}',
    portability   TEXT NOT NULL DEFAULT '',
    bundle        TEXT NOT NULL DEFAULT '',
    bundle_sha256 TEXT NOT NULL DEFAULT '',
    bundle_bytes  INTEGER NOT NULL DEFAULT 0,
    repository    TEXT NOT NULL DEFAULT '',
    homepage      TEXT NOT NULL DEFAULT '',
    version       TEXT NOT NULL DEFAULT '',
    tags          TEXT NOT NULL DEFAULT '',
    owner         TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'community',
    state         TEXT NOT NULL DEFAULT 'active',
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    approved_at   TEXT,
    approved_by   TEXT,
    review_json   TEXT,
    reviewed_at   TEXT
);
