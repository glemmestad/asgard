//! Agent seed (selective). The `seed/` folder is the offline guidance an engineer
//! drops into a repo; this module turns that hand-curated taxonomy into a
//! *selectable* one. Given the languages a repo is written in and the work being
//! done, [`plan`] returns the minimal relevant slice — core + matching language
//! add-ons + matching domain overlays + relevant templates — rather than dumping
//! everything. Served over MCP as `seed_list` / `seed_plan` / `seed_get`.
//!
//! Inspired by the agent-seed pattern (a markdown-first, layered repo-guidance
//! seed); the contribution here is the runtime selection that pattern leaves to
//! human judgment. The core files are the same ones published as `standards`, so
//! the seed and the live standards never drift.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedKind {
    /// Always-relevant operating agreement / workflow / standards.
    Core,
    /// Language-specific add-on, pulled when the repo uses that language.
    Language,
    /// Domain overlay, pulled when the work matches (frontend, ml, …).
    Domain,
    /// A fill-in artifact template (plan, threat model, change summary).
    Template,
}

impl SeedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SeedKind::Core => "core",
            SeedKind::Language => "language",
            SeedKind::Domain => "domain",
            SeedKind::Template => "template",
        }
    }
}

/// Rigor tier. A higher tier pulls in more core material and the heavier
/// templates. Mirrors agent-seed's adoption modes, collapsed to three:
/// `Minimal` ≈ try/experimental (only the always-on core, language add-ons, and
/// keyword-matched overlays — no heavyweight standards or templates by default);
/// `Standard` ≈ adopt/default (the full core operating agreement and the routine
/// templates come along); `Strict` ≈ enforce/required (additionally pulls the
/// heavy artifacts like the threat model). Domain overlays and language add-ons
/// are selected by match, not by tier, at every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeedTier {
    Minimal = 0,
    Standard = 1,
    Strict = 2,
}

impl SeedTier {
    pub fn parse(s: &str) -> Option<SeedTier> {
        match s.trim().to_lowercase().as_str() {
            "minimal" | "min" => Some(SeedTier::Minimal),
            "standard" | "std" | "" => Some(SeedTier::Standard),
            "strict" | "high" | "high-rigor" => Some(SeedTier::Strict),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            SeedTier::Minimal => "minimal",
            SeedTier::Standard => "standard",
            SeedTier::Strict => "strict",
        }
    }
}

pub struct SeedModule {
    pub id: &'static str,
    pub title: &'static str,
    pub kind: SeedKind,
    /// Suggested path to write the file to in the target repo.
    pub path: &'static str,
    /// Minimum tier at which a Core/Template module is included by default.
    pub tier: SeedTier,
    /// For `Language` modules: the language tokens that select it.
    pub languages: &'static [&'static str],
    /// For `Domain`/`Template` modules: task keywords that select it (matched as
    /// substrings of the lowercased task description).
    pub keywords: &'static [&'static str],
    pub summary: &'static str,
    pub body: &'static str,
}

pub const SEED: &[SeedModule] = &[
    // --- core (the always-on operating agreement) ---
    SeedModule {
        id: "agents",
        title: "AGENTS.md — the entry point",
        kind: SeedKind::Core,
        path: "AGENTS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[],
        summary: "The map an agent reads first: the build loop, the MCP tools, reading order, and when to stop and ask.",
        body: include_str!("../../../seed/AGENTS.md"),
    },
    SeedModule {
        id: "gitignore",
        title: ".gitignore — keep secrets out of git",
        kind: SeedKind::Core,
        path: ".gitignore",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[],
        summary: "Excludes .env and credentials (control-plane resources mint secrets locally), plus build output and editor noise. Write this first.",
        body: include_str!("../../../seed/.gitignore"),
    },
    SeedModule {
        id: "workflow",
        title: "Workflow",
        kind: SeedKind::Core,
        path: ".agent/WORKFLOW.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[],
        summary: "How the agent works: branch, register (the gate), request resources via the control plane, pass the eval/merge gate.",
        body: include_str!("../../../seed/.agent/WORKFLOW.md"),
    },
    SeedModule {
        id: "standards",
        title: "Engineering standards",
        kind: SeedKind::Core,
        path: ".agent/STANDARDS.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &[],
        summary: "Code, testing, CI, conventional commits, dependency hygiene, documentation.",
        body: include_str!("../../../seed/.agent/STANDARDS.md"),
    },
    SeedModule {
        id: "security",
        title: "Security",
        kind: SeedKind::Core,
        path: ".agent/SECURITY.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &[],
        summary: "Data classification, secrets, least privilege, the gateway, no shadow AI.",
        body: include_str!("../../../seed/.agent/SECURITY.md"),
    },
    SeedModule {
        id: "done",
        title: "Definition of done",
        kind: SeedKind::Core,
        path: ".agent/DONE.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &[],
        summary: "The done bar: gates green and actually run, behavior verified, no orphaned code, security/data-class checks, change summary.",
        body: include_str!("../../../seed/.agent/DONE.md"),
    },
    SeedModule {
        id: "prompts",
        title: "Operating prompts",
        kind: SeedKind::Core,
        path: ".agent/PROMPTS.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &[],
        summary: "Reusable prompt patterns: ask for a plan, self-review, request a resource through the control plane, write a change summary.",
        body: include_str!("../../../seed/.agent/PROMPTS.md"),
    },
    // --- language add-ons ---
    SeedModule {
        id: "lang-rust",
        title: "Rust add-on",
        kind: SeedKind::Language,
        path: ".agent/lang/RUST.md",
        tier: SeedTier::Minimal,
        languages: &["rust", "rs", "cargo"],
        keywords: &[],
        summary: "Rust done-bar (fmt/clippy/test), error handling, async, unsafe, dependency hygiene.",
        body: include_str!("../../../seed/.agent/lang/RUST.md"),
    },
    SeedModule {
        id: "lang-python",
        title: "Python add-on",
        kind: SeedKind::Language,
        path: ".agent/lang/PYTHON.md",
        tier: SeedTier::Minimal,
        languages: &["python", "py"],
        keywords: &[],
        summary: "Python done-bar (ruff/types/pytest), typing, models over dicts, exceptions, secrets.",
        body: include_str!("../../../seed/.agent/lang/PYTHON.md"),
    },
    SeedModule {
        id: "lang-typescript",
        title: "TypeScript / JavaScript add-on",
        kind: SeedKind::Language,
        path: ".agent/lang/TYPESCRIPT.md",
        tier: SeedTier::Minimal,
        languages: &[
            "typescript",
            "ts",
            "javascript",
            "js",
            "node",
            "react",
            "vue",
            "svelte",
        ],
        keywords: &[],
        summary: "TS/JS done-bar (tsc/lint/test), no-any, boundary validation, supply-chain, async hygiene.",
        body: include_str!("../../../seed/.agent/lang/TYPESCRIPT.md"),
    },
    SeedModule {
        id: "lang-go",
        title: "Go add-on",
        kind: SeedKind::Language,
        path: ".agent/lang/GO.md",
        tier: SeedTier::Minimal,
        languages: &["go", "golang"],
        keywords: &[],
        summary: "Go done-bar (vet/lint/race), error wrapping, context, concurrency discipline, layout.",
        body: include_str!("../../../seed/.agent/lang/GO.md"),
    },
    SeedModule {
        id: "lang-terraform",
        title: "Terraform / IaC add-on",
        kind: SeedKind::Language,
        path: ".agent/lang/TERRAFORM.md",
        tier: SeedTier::Minimal,
        languages: &["terraform", "tf", "hcl"],
        keywords: &[],
        summary: "IaC done-bar (fmt/validate/reviewed plan), no secrets in state, module contracts, existing-VPC inputs, cost tags, idempotency, destroy-safety.",
        body: include_str!("../../../seed/.agent/lang/TERRAFORM.md"),
    },
    // --- domain overlays ---
    SeedModule {
        id: "domain-frontend",
        title: "Frontend domain overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/FRONTEND.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "frontend", "ui", "web", "react", "vue", "svelte", "css", "html",
            "component", "dashboard", "page", "browser",
        ],
        summary: "Accessibility as a requirement, browser-verify the four states, state separation, bundle/XSS safety.",
        body: include_str!("../../../seed/.agent/domains/FRONTEND.md"),
    },
    SeedModule {
        id: "domain-ai-ml",
        title: "AI / ML domain overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/AI_ML.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "ml", "model", "llm", "agent", "prompt", "inference", "training",
            "train", "eval", "embedding", "rag", "ai", "fine-tune", "finetune",
        ],
        summary: "Route models through the gateway, reproducibility, evals as the test suite, prompt-injection safety.",
        body: include_str!("../../../seed/.agent/domains/AI_ML.md"),
    },
    SeedModule {
        id: "domain-systems",
        title: "Systems / backend-service overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/SYSTEMS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "api", "service", "backend", "server", "microservice", "endpoint",
            "grpc", "rest", "database", "queue", "infra", "infrastructure",
        ],
        summary: "API contracts & migrations, failure as default (timeouts/retries/idempotency), observability, resource lifecycles.",
        body: include_str!("../../../seed/.agent/domains/SYSTEMS.md"),
    },
    SeedModule {
        id: "domain-data",
        title: "Data pipelines overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/DATA_PIPELINES.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "pipeline", "etl", "elt", "data", "ingest", "ingestion", "batch",
            "stream", "warehouse", "analytics", "spark", "airflow", "dbt",
        ],
        summary: "Idempotent reprocessing, data quality at ingestion, provenance & cost, additive schema evolution.",
        body: include_str!("../../../seed/.agent/domains/DATA_PIPELINES.md"),
    },
    SeedModule {
        id: "domain-quantum",
        title: "Quantum engineering overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/QUANTUM.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "quantum", "qubit", "photonic", "circuit", "qpu", "qec",
            "error-correction", "lattice", "stabilizer", "gate-synthesis",
            "simulation",
        ],
        summary: "State conventions explicitly, reproduce against an independent reference, seed stochastic runs, document the simulation regime and noise/QEC assumptions.",
        body: include_str!("../../../seed/.agent/domains/QUANTUM.md"),
    },
    SeedModule {
        id: "domain-compilers",
        title: "Compilers & language tooling overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/COMPILERS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "compiler", "parser", "lexer", "ir", "codegen", "ast",
            "optimization", "transpiler", "llvm", "mlir", "bytecode",
        ],
        summary: "Semantics-preserving passes, stated pre/post invariants, parser robustness, IR verification, golden + differential tests.",
        body: include_str!("../../../seed/.agent/domains/COMPILERS.md"),
    },
    SeedModule {
        id: "domain-accelerators",
        title: "Hardware accelerators overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/ACCELERATORS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "gpu", "cuda", "fpga", "asic", "accelerator", "kernel", "hls",
            "verilog", "vhdl", "rtl", "simd",
        ],
        summary: "Validate against a reference, correct synchronization, stated precision contract, measure on target hardware, account for data movement.",
        body: include_str!("../../../seed/.agent/domains/ACCELERATORS.md"),
    },
    SeedModule {
        id: "domain-formal-verification",
        title: "Formal verification overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/FORMAL_VERIFICATION.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "formal", "proof", "verification", "smt", "tla", "coq", "lean",
            "model-checking", "invariant", "theorem",
        ],
        summary: "Human-readable spec, stated assumptions and trusted base, no admitted obligations, bounded-vs-exhaustive declared, vacuity ruled out.",
        body: include_str!("../../../seed/.agent/domains/FORMAL_VERIFICATION.md"),
    },
    SeedModule {
        id: "domain-autonomous-research",
        title: "Autonomous research overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/AUTONOMOUS_RESEARCH.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "research", "experiment", "autonomous", "agentic", "hypothesis",
            "reproducibility", "notebook", "dataset", "sweep",
        ],
        summary: "Reproducible from (code, data, params, seed), pinned environment, hypothesis stated up front, bounded sweeps, cost-attributed compute, capped agentic loops.",
        body: include_str!("../../../seed/.agent/domains/AUTONOMOUS_RESEARCH.md"),
    },
    SeedModule {
        id: "domain-systems-kernels",
        title: "Systems & kernels overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/SYSTEMS_KERNELS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "kernel", "driver", "embedded", "firmware", "rtos", "syscall",
            "interrupt", "dma", "bootloader", "no-std", "real-time",
        ],
        summary: "Bounds/lifetime-accounted memory, isolated unsafe, short reentrant interrupt handlers, stated lock order, no hot-path allocation, bounded stack & deadlines.",
        body: include_str!("../../../seed/.agent/domains/SYSTEMS_KERNELS.md"),
    },
    SeedModule {
        id: "domain-robotics",
        title: "Robotics & control overlay",
        kind: SeedKind::Domain,
        path: ".agent/domains/ROBOTICS.md",
        tier: SeedTier::Minimal,
        languages: &[],
        keywords: &[
            "robot", "robotics", "ros", "control", "sensor", "actuator",
            "slam", "kinematics", "motion-planning", "teleop",
        ],
        summary: "Defined safe state and fault paths, bounded actuator commands, explicit frames/units, deadline-meeting control loop, sim before supervised hardware.",
        body: include_str!("../../../seed/.agent/domains/ROBOTICS.md"),
    },
    // --- templates ---
    SeedModule {
        id: "template-exec-plan",
        title: "Execution plan template",
        kind: SeedKind::Template,
        path: ".agent/templates/EXEC_PLAN.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &["plan"],
        summary: "Fill in before editing non-trivial work: goal, context, approach, steps, verification, risks.",
        body: include_str!("../../../seed/.agent/templates/EXEC_PLAN.md"),
    },
    SeedModule {
        id: "template-change-summary",
        title: "Change summary template",
        kind: SeedKind::Template,
        path: ".agent/templates/CHANGE_SUMMARY.md",
        tier: SeedTier::Standard,
        languages: &[],
        keywords: &[],
        summary: "Produce on completion: what changed, why, how verified, risk & rollout, follow-ups.",
        body: include_str!("../../../seed/.agent/templates/CHANGE_SUMMARY.md"),
    },
    SeedModule {
        id: "template-threat-model",
        title: "Threat model template",
        kind: SeedKind::Template,
        path: ".agent/templates/THREAT_MODEL.md",
        tier: SeedTier::Strict,
        languages: &[],
        keywords: &[
            "security", "auth", "secret", "crypto", "login", "token", "oauth",
            "permission", "untrusted",
        ],
        summary: "For security-sensitive work: assets, trust boundaries, threats, decisions, residual risk.",
        body: include_str!("../../../seed/.agent/templates/THREAT_MODEL.md"),
    },
];

pub fn all() -> &'static [SeedModule] {
    SEED
}

pub fn get(id: &str) -> Option<&'static SeedModule> {
    SEED.iter().find(|m| m.id == id)
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Split a task description into lowercased word tokens, so keyword matching is
/// whole-word (avoids e.g. "ui" matching inside "build").
fn task_tokens(task: &str) -> Vec<String> {
    task.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn keyword_hits(keywords: &[&str], tokens: &[String], task_l: &str) -> bool {
    keywords.iter().any(|k| {
        if k.contains('-') {
            task_l.contains(k)
        } else {
            tokens.iter().any(|t| t == k)
        }
    })
}

/// Select the minimal relevant seed slice for a piece of work. `languages` are
/// the languages the repo is written in; `task` is a free-text description of the
/// work; `tier` gates how much core material and which templates come along.
///
/// A module is included when it is core at-or-below `tier`, a language add-on
/// matching one of `languages`, a domain overlay whose keywords appear in `task`,
/// or a template that is either in-tier or keyword-matched by `task`.
pub fn plan(languages: &[String], task: &str, tier: SeedTier) -> Vec<&'static SeedModule> {
    let langs: Vec<String> = languages.iter().map(|l| norm(l)).collect();
    let task_l = norm(task);
    let tokens = task_tokens(task);
    SEED.iter()
        .filter(|m| match m.kind {
            SeedKind::Core => m.tier <= tier,
            SeedKind::Language => m.languages.iter().any(|l| langs.iter().any(|r| r == l)),
            SeedKind::Domain => keyword_hits(m.keywords, &tokens, &task_l),
            SeedKind::Template => m.tier <= tier || keyword_hits(m.keywords, &tokens, &task_l),
        })
        .collect()
}

fn adoption_mode(tier: SeedTier) -> &'static str {
    match tier {
        SeedTier::Minimal => "try",
        SeedTier::Standard => "adopt",
        SeedTier::Strict => "enforce",
    }
}

/// Emit a record of which language add-ons and domain overlays a `plan` adopted,
/// and at which mode — analogous to agent-seed's DOMAIN_OVERLAY_ADOPTION note an
/// engineer drops in a repo to declare what guidance is in force. The mode tracks
/// the rigor `tier` the plan was built at.
pub fn adoption_record(plan: &[&SeedModule], tier: SeedTier) -> String {
    let mode = adoption_mode(tier);
    let mut out = format!(
        "# Seed adoption record\n\nTier: {} (mode: {})\n",
        tier.as_str(),
        mode
    );
    let mut emit = |heading: &str, kind: SeedKind| {
        let lines: Vec<String> = plan
            .iter()
            .filter(|m| m.kind == kind)
            .map(|m| format!("- {} ({}) — {}", m.id, mode, m.title))
            .collect();
        if !lines.is_empty() {
            out.push_str(&format!("\n## {}\n{}\n", heading, lines.join("\n")));
        }
    };
    emit("Language add-ons", SeedKind::Language);
    emit("Domain overlays", SeedKind::Domain);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_backend_pulls_rust_and_systems_not_frontend() {
        let p = plan(
            &["rust".into()],
            "build a REST API service",
            SeedTier::Standard,
        );
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"agents"));
        assert!(ids.contains(&"standards")); // standard tier
        assert!(ids.contains(&"lang-rust"));
        assert!(ids.contains(&"domain-systems"));
        assert!(!ids.contains(&"domain-frontend"));
        assert!(!ids.contains(&"lang-python"));
    }

    #[test]
    fn react_frontend_pulls_typescript_and_frontend() {
        let p = plan(
            &["typescript".into()],
            "a React dashboard page",
            SeedTier::Minimal,
        );
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"lang-typescript"));
        assert!(ids.contains(&"domain-frontend"));
        // Minimal tier: STANDARDS/SECURITY and templates are not auto-included.
        assert!(!ids.contains(&"standards"));
        assert!(!ids.contains(&"template-exec-plan"));
    }

    #[test]
    fn ml_work_pulls_ai_overlay_and_python() {
        let p = plan(
            &["python".into()],
            "train a model and add evals",
            SeedTier::Standard,
        );
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"lang-python"));
        assert!(ids.contains(&"domain-ai-ml"));
    }

    #[test]
    fn strict_security_work_pulls_threat_model() {
        let p = plan(&["go".into()], "add OAuth login", SeedTier::Strict);
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"template-threat-model")); // strict tier AND keyword
        assert!(ids.contains(&"security"));
        assert!(ids.contains(&"lang-go"));
    }

    #[test]
    fn js_alias_selects_typescript_addon() {
        let p = plan(&["js".into()], "node service", SeedTier::Minimal);
        assert!(p.iter().any(|m| m.id == "lang-typescript"));
    }

    #[test]
    fn bare_bootstrap_inlines_the_agents_entry_point() {
        // The no-args `bootstrap` call (no languages, no task) must still return the
        // AGENTS.md entry point with its real inlined body — that's the whole
        // "set up a repo in one call". A regression guard against it handing back an
        // empty/stub plan that "talks about the seed" without pulling files over.
        let p = plan(&[], "", SeedTier::Standard);
        let agents = p
            .iter()
            .find(|m| m.id == "agents")
            .expect("AGENTS.md (id 'agents') must be in the bare plan");
        assert_eq!(agents.path, "AGENTS.md");
        assert!(
            agents.body.len() > 500 && agents.body.contains("register_project"),
            "AGENTS.md body must be the real inlined doc, not a stub"
        );
    }

    #[test]
    fn terraform_repo_pulls_terraform_addon() {
        let p = plan(
            &["terraform".into()],
            "provision a bucket",
            SeedTier::Minimal,
        );
        assert!(p.iter().any(|m| m.id == "lang-terraform"));
        let p2 = plan(&["tf".into()], "spin up infra", SeedTier::Minimal);
        assert!(p2.iter().any(|m| m.id == "lang-terraform"));
    }

    #[test]
    fn quantum_task_pulls_quantum_overlay() {
        let p = plan(
            &["python".into()],
            "build a quantum circuit simulator",
            SeedTier::Minimal,
        );
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"domain-quantum"));
        assert!(!ids.contains(&"domain-compilers"));
    }

    #[test]
    fn compiler_task_pulls_compilers_overlay() {
        let p = plan(
            &["rust".into()],
            "write a compiler optimization pass over the IR",
            SeedTier::Minimal,
        );
        let ids: Vec<&str> = p.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"domain-compilers"));
        assert!(!ids.contains(&"domain-quantum"));
    }

    #[test]
    fn done_and_prompts_appear_at_standard_not_minimal() {
        let std = plan(&["rust".into()], "refactor a module", SeedTier::Standard);
        let std_ids: Vec<&str> = std.iter().map(|m| m.id).collect();
        assert!(std_ids.contains(&"done"));
        assert!(std_ids.contains(&"prompts"));

        let min = plan(&["rust".into()], "refactor a module", SeedTier::Minimal);
        let min_ids: Vec<&str> = min.iter().map(|m| m.id).collect();
        assert!(!min_ids.contains(&"done"));
        assert!(!min_ids.contains(&"prompts"));
    }

    #[test]
    fn adoption_record_lists_selected_addons_and_overlays() {
        let p = plan(
            &["terraform".into()],
            "provision a GPU kernel runner",
            SeedTier::Standard,
        );
        let rec = adoption_record(&p, SeedTier::Standard);
        assert!(rec.contains("Tier: standard (mode: adopt)"));
        assert!(rec.contains("lang-terraform (adopt)"));
        assert!(rec.contains("domain-accelerators (adopt)"));
        // Core modules are not part of the adoption record.
        assert!(!rec.contains("standards ("));
    }
}
