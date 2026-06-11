//! Starter guidance + recipes shipped with the binary, adapted for Frontkeep's
//! primitives. Seeded into an empty store on boot (see `seed_knowledge`) so a
//! fresh deploy isn't blank; never overwrites human- or agent-authored content.

/// (title, summary, tags, markdown body). Substantial, vendor-neutral playbooks —
/// the kind of "how to do AI/agent work well" content an enterprise wants out of
/// the gate, and an example to keep building from.
pub const GUIDANCE: &[(&str, &str, &[&str], &str)] = &[
    (
        "Choosing a Model",
        "Pick the cheapest model that clears your data-sensitivity floor and capability bar.",
        &["models", "cost", "inference"],
        include_str!("../../../seed/knowledge/guidance/choosing-a-model.md"),
    ),
    (
        "Writing Good Evals",
        "Build evals as regression tests; 20-50 sharp cases beat 1000 random ones.",
        &["evals", "testing", "quality"],
        include_str!("../../../seed/knowledge/guidance/writing-good-evals.md"),
    ),
    (
        "RAG Patterns",
        "When retrieval helps, why chunking beats the embedding model, and citation validation.",
        &["rag", "retrieval", "grounding"],
        include_str!("../../../seed/knowledge/guidance/rag-patterns.md"),
    ),
    (
        "Agent Orchestration",
        "Default to the simplest of pipeline, single-agent, or multi-agent that works.",
        &["agents", "orchestration", "tools"],
        include_str!("../../../seed/knowledge/guidance/agent-orchestration.md"),
    ),
    (
        "Long Context and Caching",
        "Choose between stuffing context, RAG, and hierarchical; cache prefixes, beat lost-in-the-middle.",
        &["context", "caching", "rag"],
        include_str!("../../../seed/knowledge/guidance/long-context-and-caching.md"),
    ),
    (
        "Cost Optimization",
        "Measure first; the biggest levers are model choice, caching, batching, and idle compute.",
        &["cost", "budget", "efficiency"],
        include_str!("../../../seed/knowledge/guidance/cost-optimization.md"),
    ),
    (
        "Autonomous Research Loops",
        "Design hypothesis-plan-run-grade loops with hard guardrails and honest self-grading.",
        &["agents", "research", "evals"],
        include_str!("../../../seed/knowledge/guidance/autonomous-research-loops.md"),
    ),
    (
        "Handling Secrets",
        "Values live in the secret store; fetch at runtime, least privilege, plan for rotation.",
        &["secrets", "security", "secrets-management"],
        include_str!("../../../seed/knowledge/guidance/handling-secrets.md"),
    ),
    (
        "Picking a Classification",
        "Classify by the most sensitive data touched; over- and under-classifying both cost you.",
        &["classification", "governance", "data-sensitivity"],
        include_str!("../../../seed/knowledge/guidance/picking-a-classification.md"),
    ),
];

/// (name, summary, tags, markdown runbook body, spec JSON at-a-glance)
pub const RECIPES: &[(&str, &str, &[&str], &str, &str)] = &[
    (
        "Add real-time collaboration to your app",
        "From 'I want multi-user live editing' to 'it's working' — the server you build, the platform primitives you provision, and how to verify.",
        &["collaboration", "realtime", "recipe"],
        include_str!("../../../seed/knowledge/recipes/realtime-collab.md"),
        include_str!("../../../seed/knowledge/recipes/realtime-collab.json"),
    ),
    (
        "Stand up an authenticated MCP server",
        "From 'I have tool code' to agents calling https://…/mcp with a Bearer token — the image, the Auth0 app, the HTTPS service.",
        &["mcp", "auth0", "recipe"],
        include_str!("../../../seed/knowledge/recipes/mcp-server-with-auth0.md"),
        include_str!("../../../seed/knowledge/recipes/mcp-server-with-auth0.json"),
    ),
];

/// (name, summary, install-spec JSON, tags, README markdown). A few well-known
/// MCP servers, seeded as the company-approved tier (owner `frontkeep`) so a fresh
/// deploy's MCP catalog isn't blank and shows both transports (stdio + remote).
/// The structured install renders to per-client snippets (Claude Code/Codex/Cursor).
pub const MCP_SERVERS: &[(&str, &str, &str, &[&str], &str)] = &[
    (
        "GitHub",
        "Repos, issues, pull requests, and code search as agent tools.",
        r#"{"transport":"stdio","command":"docker","args":["run","-i","--rm","-e","GITHUB_PERSONAL_ACCESS_TOKEN","ghcr.io/github/github-mcp-server"],"env":["GITHUB_PERSONAL_ACCESS_TOKEN"]}"#,
        &["github", "vcs", "official"],
        include_str!("../../../seed/knowledge/mcp_servers/github.md"),
    ),
    (
        "Filesystem",
        "Sandboxed read/write access to a directory you choose — local files as tools.",
        r#"{"transport":"stdio","command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","/path/to/allowed/dir"],"env":[]}"#,
        &["files", "local", "official"],
        include_str!("../../../seed/knowledge/mcp_servers/filesystem.md"),
    ),
    (
        "Frontkeep control plane",
        "This hub's own MCP server — register projects, provision services, read cost, pull standards/guidance/recipes.",
        r#"{"transport":"remote","url":"https://frontkeep.example.com/mcp"}"#,
        &["frontkeep", "governance", "remote"],
        include_str!("../../../seed/knowledge/mcp_servers/frontkeep.md"),
    ),
];

/// One seeded skill: (name, summary, runtime, tags, files as (path, text)).
type SeedSkill = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, &'static str)],
);

/// Generic, vendor-neutral example skills seeded as the company-approved tier so a
/// fresh Skills catalog isn't blank and shows both shapes: an instructions-only skill
/// that drives an external tool, and one that bundles a script. Portability + manifest
/// are derived on seed.
pub const SKILLS: &[SeedSkill] = &[
    (
        "Changelog from commits",
        "Draft a release changelog from the git log since the last tag, grouped by change type.",
        "claude-code",
        &["git", "release", "docs"],
        &[(
            "SKILL.md",
            include_str!("../../../seed/knowledge/skills/changelog-writer/SKILL.md"),
        )],
    ),
    (
        "CSV profiler",
        "Summarize a CSV — column types, null counts, ranges, and obvious data-quality flags.",
        "claude-code",
        &["data", "csv", "analysis"],
        &[
            (
                "SKILL.md",
                include_str!("../../../seed/knowledge/skills/csv-profiler/SKILL.md"),
            ),
            (
                "scripts/profile.py",
                include_str!("../../../seed/knowledge/skills/csv-profiler/scripts/profile.py"),
            ),
        ],
    ),
];
