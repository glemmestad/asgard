//! The Asgard binary: one statically-linked entrypoint that wires every layer,
//! serves REST + GraphQL + the embedded web UI, reconciles catalog sources, and
//! exposes the MCP server and CLI client commands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use clap::{Parser, Subcommand};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use asgard_api::AppState;
use asgard_catalog::{
    CatalogRepo, FixtureProvider, GitHubProvider, GitLabProvider, SchemaRegistry, SourceProvider,
};
use asgard_gateway::{
    AnthropicProvider, Gateway, GatewayRepo, MockProvider, Mode, ModelInfo, ModelRegistry,
    OpenAiProvider, Provider,
};
use asgard_identity::oidc::OidcConfig;
use asgard_identity::{IdentityService, OidcRoleConfig};
use asgard_policy::{CedarEngine, PolicyEngine};
use asgard_provision::{
    AutoApprovePolicy, AwsCostExplorerSource, BuiltinSecretStore, CloudTarget,
    DatabricksCostSource, ExecConnector, ExecCostSource, GatewaySource, LiteLlmConnector,
    LiteLlmCostSource, ProvisionRepo, ProvisionService, RunLogStore, SecretStore, ServiceCatalog,
    StubProvisioner, TerraformConnector, TfStateStore, DEV_SECRET_KEY,
};
use asgard_registry::{
    ClassificationRequirements, GovernanceConfig, GroupAllowlist, GroupEntry, ProjectRegistry,
    RegistrationPolicy, ReviewConfig,
};
use asgard_reviewer::{
    CodeReview, LlmJudge, RegistryStandards, ReviewDepth, ReviewDepthMap, ReviewService,
    ReviewerCatalog, ReviewerRegistry, WebhookReviewer,
};
use asgard_storage::{leases::Leases, Db};
use asgard_workflow::WorkflowEngine;

use asgard_cli::config::Resolved;
use asgard_cli::mcp::McpClient;
use asgard_cli::render::{render, Output, Shape};

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

// The Docusaurus site, served at /docs. It's produced by a Node build
// (`docs/build`), which exists in the release image (the Dockerfile builds it
// before the binary) but not on a bare `cargo build` — `allow_missing` keeps
// those builds compiling, and the handler serves a placeholder when empty.
#[derive(RustEmbed)]
#[folder = "../../docs/build"]
#[allow_missing = true]
struct DocsAssets;

#[derive(Parser)]
#[command(
    name = "asgard",
    version,
    about = "Control plane for AI/agent development — server (serve/mcp) plus a PAT-authed CLI"
)]
struct Cli {
    /// Database URL for the in-process server commands (`serve`, `mcp`).
    #[arg(
        long,
        env = "ASGARD_DATABASE_URL",
        default_value = "sqlite://asgard.db",
        global = true
    )]
    database_url: String,
    /// Asgard server origin for CLI commands (talks to `/mcp`). Falls back to the
    /// selected profile, then `http://localhost:8080`.
    #[arg(long, env = "ASGARD_URL", global = true)]
    url: Option<String>,
    /// User PAT (`asg_pat_…`) authenticating CLI commands. Falls back to the
    /// selected profile. Mint one in the UI under "Get Started".
    #[arg(long, env = "ASGARD_PAT", global = true)]
    pat: Option<String>,
    /// Config profile to use (see `asgard login`); defaults to the file's default.
    #[arg(long, env = "ASGARD_PROFILE", global = true)]
    profile: Option<String>,
    /// Output format for CLI commands: json, table (default), or yaml.
    #[arg(long, short = 'o', env = "ASGARD_OUTPUT", global = true)]
    output: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server: REST + GraphQL + embedded web UI.
    Serve {
        #[arg(long, env = "ASGARD_BIND", default_value = "0.0.0.0:8080")]
        bind: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the MCP server over stdio.
    Mcp {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Write a starter asgard.yaml into the current directory.
    Init,
    /// Save a server URL + PAT into a config profile (named by the global --profile).
    Login {
        /// Don't make the written profile the default.
        #[arg(long)]
        no_default: bool,
    },
    /// List every tool the server exposes (the live parity surface).
    Tools,
    /// Call any MCP tool by name — guarantees parity with the agent surface.
    Call {
        /// Tool name (see `asgard tools`).
        tool: String,
        /// Arguments as a JSON object ("-" reads stdin). Mutually exclusive with --arg.
        #[arg(long, conflicts_with = "arg")]
        json: Option<String>,
        /// Repeated key=value args; each value is parsed as JSON if it parses, else a string.
        #[arg(long = "arg", value_name = "KEY=VALUE")]
        arg: Vec<String>,
    },
    /// Generate a shell completion script.
    Completions {
        /// Shell: bash, zsh, fish, powershell, or elvish.
        shell: clap_complete::Shell,
    },
    /// Validate a manifest file against its schema (offline).
    Validate { path: PathBuf },
    /// Scaffold a golden-path agent template (offline).
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Service catalog + entity discovery.
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCmd,
    },
    /// Project registration, lifecycle, credentials, and promotion (the gate).
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Enterprise standards an agent's output must conform to.
    Standards {
        #[command(subcommand)]
        cmd: StandardsCmd,
    },
    /// Governed how-to guidance docs.
    Guidance {
        #[command(subcommand)]
        cmd: GuidanceCmd,
    },
    /// Narrated runbooks (recipes).
    Recipe {
        #[command(subcommand)]
        cmd: RecipeCmd,
    },
    /// The published MCP-server catalog.
    McpCatalog {
        #[command(subcommand)]
        cmd: McpCatalogCmd,
    },
    /// The published skills catalog (publish/install/export agent skills).
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Cost + spend reporting.
    Cost {
        #[command(subcommand)]
        cmd: CostCmd,
    },
    /// Org-wide governance / portfolio metrics.
    Governance,
    /// Infrastructure resources for a project.
    Resource {
        #[command(subcommand)]
        cmd: ResourceCmd,
    },
    /// Project secrets.
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Agent-seed modules (the standards files for a repo).
    Seed {
        #[command(subcommand)]
        cmd: SeedCmd,
    },
    /// One-shot repo seed: AGENTS.md + standards (dry-run unless --write).
    Bootstrap {
        #[arg(long)]
        languages: Vec<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        tier: Option<String>,
        /// Actually write the files (default: dry-run).
        #[arg(long)]
        write: bool,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
        /// Destination directory (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Run inference through a project's gateway key (minted/reused for you).
    Chat {
        #[arg(long)]
        project: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        message: String,
        #[arg(long)]
        max_tokens: Option<u32>,
        #[arg(long)]
        temperature: Option<f32>,
        #[arg(long)]
        data_class: Option<String>,
    },
}

#[derive(Subcommand)]
enum CatalogCmd {
    /// Search catalog entities.
    Search {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        query: Option<String>,
    },
    /// Get one entity by kind/namespace/name.
    Get {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        name: String,
    },
    /// List provisionable services.
    Services,
    /// Show one service manifest by id.
    Service {
        #[arg(long)]
        id: String,
    },
    /// List cost-center groups available for registration.
    Groups,
}

#[derive(Subcommand)]
enum AgentCmd {
    New {
        #[arg(long, default_value = "code-review")]
        template: String,
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// List the projects you own or manage.
    Ls,
    /// Register a project; mints a stable proj-YYYY-NNNN id.
    Register {
        #[arg(long)]
        name: String,
        /// Owner email (ignored on a user PAT — stamped from the authenticated user).
        #[arg(long, default_value = "")]
        owner: String,
        #[arg(long, default_value = "")]
        manager: String,
        #[arg(long, default_value = "")]
        group: String,
        #[arg(long)]
        classification: Option<String>,
        #[arg(long)]
        data_class: Option<String>,
        #[arg(long)]
        budget_usd: Option<f64>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Update a project's mutable fields (id never changes).
    Update {
        project_id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        budget_usd: Option<f64>,
    },
    /// Show a project's registration record.
    Get { project_id: String },
    /// Show a project's runtime state (budget, spend, kill switch).
    State { project_id: String },
    /// Mint the project's gateway LLM key.
    Credential { project_id: String },
    /// Show the promotion checklist.
    Promotion { project_id: String },
    /// Request a one-step promotion to the next tier.
    Promote {
        project_id: String,
        #[arg(long)]
        target: String,
    },
    /// Forward a flagged promotion request to a human.
    Escalate {
        #[arg(long)]
        request_id: String,
    },
}

#[derive(Subcommand)]
enum StandardsCmd {
    /// List enterprise standard sets.
    Ls,
    /// Fetch the full text of a standard set.
    Get { id: String },
}

#[derive(Subcommand)]
enum GuidanceCmd {
    /// List guidance docs.
    Ls,
    /// Fetch one guidance doc by slug.
    Get { slug: String },
    /// Create or update a guidance doc.
    Put {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RecipeCmd {
    /// List recipes.
    Ls,
    /// Fetch one recipe by slug.
    Get { slug: String },
    /// Create or update a recipe.
    Put {
        #[arg(long)]
        name: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        /// Optional machine-readable spec, as a JSON object.
        #[arg(long)]
        spec: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
}

#[derive(Subcommand)]
enum McpCatalogCmd {
    /// List published MCP servers.
    Ls,
    /// Fetch one catalog entry by id.
    Get { id: String },
    /// Publish or update an MCP server entry.
    Publish {
        #[arg(long)]
        name: String,
        /// Provide to update an entry you own; omit to publish a new one.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        readme: Option<String>,
        /// Structured install spec, as a JSON object.
        #[arg(long)]
        install: Option<String>,
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        homepage: Option<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Change an entry's lifecycle: active, disabled, or archived.
    SetState {
        #[arg(long)]
        id: String,
        #[arg(long)]
        state: String,
    },
}

#[derive(Subcommand)]
enum SkillsCmd {
    /// List published skills.
    Ls,
    /// Fetch one skill catalog entry by id.
    Get { id: String },
    /// Publish or update a skill. Bundle source: --dir or --bundle (exactly one).
    Publish {
        /// Provide to update a skill you own; omit to publish a new one.
        #[arg(long)]
        id: Option<String>,
        /// Skill folder on disk; every file is base64-encoded into the bundle.
        #[arg(long, conflicts_with = "bundle")]
        dir: Option<PathBuf>,
        /// Raw bundle JSON array [{path,content_b64}] ("-" reads stdin).
        #[arg(long)]
        bundle: Option<String>,
        /// Falls back to the SKILL.md frontmatter `name`.
        #[arg(long, default_value = "")]
        name: String,
        /// Falls back to the SKILL.md frontmatter `description`.
        #[arg(long, default_value = "")]
        summary: String,
        #[arg(long, default_value = "")]
        readme: String,
        /// Authored runtime: claude-code (default) or codex.
        #[arg(long, default_value = "")]
        runtime: String,
        #[arg(long, default_value = "")]
        repository: String,
        #[arg(long, default_value = "")]
        homepage: String,
        #[arg(long, default_value = "")]
        version: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Change a skill's lifecycle: active, disabled, or archived.
    SetState {
        #[arg(long)]
        id: String,
        #[arg(long)]
        state: String,
    },
    /// Export a skill bundle (optionally translated) and write it to disk.
    Export {
        id: String,
        /// Target runtime: claude-code or codex. Defaults to the skill's own.
        #[arg(long)]
        runtime: Option<String>,
        /// Output directory (default: current directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Install a skill into a runtime's skills directory on disk.
    Install {
        id: String,
        /// Destination: claude-code, codex, or cursor. Defaults to the skill's own runtime.
        #[arg(long)]
        dest: Option<String>,
        /// Override the install directory (default: the runtime's standard dir).
        #[arg(long)]
        dest_dir: Option<PathBuf>,
        /// Plan only; don't write.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum CostCmd {
    /// Model/token spend by dimension.
    Report {
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Full cost for one project (model + infra).
    Project {
        project_id: String,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
    },
    /// Daily rollup series for one project.
    Series {
        #[arg(long)]
        project: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Rollup spend grouped by a dimension.
    By {
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Latest end-of-month forecast for a project.
    Forecast { project_id: String },
    /// Recent cost anomalies.
    Anomalies {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Org cost tree for the month.
    Tree {
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Top movers vs the previous month.
    Movers {
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long)]
        top: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ResourceCmd {
    /// Request an infrastructure resource.
    Request {
        #[arg(long)]
        resource_type: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        project: Option<String>,
        /// Spec as a JSON object.
        #[arg(long)]
        spec: Option<String>,
    },
    /// Grant one resource access to another.
    Grant {
        #[arg(long)]
        consumer: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        level: Option<String>,
    },
    /// List provisioned resources.
    Ls {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
    },
    /// Fetch one resource (poll provisioning/teardown).
    Get { resource_id: String },
    /// Read the captured connector run-log for a resource (audit; admin/finance).
    Runs { resource_id: String },
    /// Retry a failed provision/teardown now (bypass the auto-retry backoff window).
    Retry { resource_id: String },
    /// Tear down a resource.
    Deprovision { resource_id: String },
    /// Roll a container service onto a new image (env/grants/cert preserved).
    DeployImage {
        /// The provisioned service to roll — its id from `resource ls`.
        resource_id: String,
        /// New image reference, e.g. <acct>.dkr.ecr.<region>.amazonaws.com/<repo>:<sha>.
        #[arg(long)]
        image: String,
        /// Target project (required on a user PAT; omit on a project key).
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Fetch a secret value (audited).
    Get {
        #[arg(long)]
        name: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Rotate a secret to a fresh value.
    Rotate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// List secret metadata (never values).
    Ls {
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum SeedCmd {
    /// List every agent-seed module.
    Ls,
    /// Plan the minimal seed file set for a repo.
    Plan {
        #[arg(long)]
        languages: Vec<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        tier: Option<String>,
    },
    /// Fetch one seed module's body.
    Get { id: String },
    /// Plan + write the seed files to disk (dry-run unless --write).
    Apply {
        #[arg(long)]
        languages: Vec<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Config {
    #[serde(default)]
    sources: Vec<SourceCfg>,
    #[serde(default)]
    reconcile_secs: Option<u64>,
    /// Seconds a fast provision request waits inline for its apply before
    /// returning the `provisioning` record to poll. Absent = 5. `long_running`
    /// services ignore it and return immediately.
    #[serde(default)]
    provision_wait_secs: Option<u64>,
    /// Fleet-wide cap on automatic provisioning retries. Absent = 5; 0 disables
    /// auto-retry everywhere. A service manifest's `retry.max_attempts` overrides
    /// this per service.
    #[serde(default)]
    provision_max_retries: Option<u32>,
    /// How often the provisioning reconciler re-drives orphaned/stale work-state
    /// rows and enqueues approved-but-recordless requests. Absent = 60.
    #[serde(default)]
    provision_reconcile_secs: Option<u64>,
    /// Lease TTL (seconds) for cross-instance coordination — the leader-leased
    /// background loops and the per-resource Terraform lock. Absent = 600. Only
    /// relevant when running more than one replica (Postgres).
    #[serde(default)]
    lease_ttl_secs: Option<u64>,
    /// Operator-configured cost-centers a project may register against. Empty =
    /// open mode (any group accepted, recorded as-is).
    #[serde(default)]
    groups: Vec<GroupEntry>,
    /// Which registration fields are mandatory. Defaults preserve the strict
    /// posture (manager + group required); relax it so a solo founder can
    /// self-register.
    #[serde(default)]
    registration: RegistrationCfg,
    /// Per-tier evidence requirements for promotion, keyed by target tier
    /// (`light-operational` / `wide-operational` / `critical-path`) to a list of
    /// required field names. Any tier present overrides that tier's shipped
    /// default; absent tiers keep the policy-doc default (mirrors `groups`'
    /// empty=default posture).
    #[serde(default)]
    classification_requirements: std::collections::BTreeMap<String, Vec<String>>,
    /// Per-tier code-review depth, keyed by target tier (`light-operational` /
    /// `wide-operational` / `critical-path`): which standard ids to judge against,
    /// the tool-loop round budget, and whether to skip. Any tier present overrides
    /// the shipped default (POC skip; Light `coding`; Wide +`security`; Critical
    /// +`workflow`); absent tiers keep the default (mirrors
    /// `classification_requirements`' empty=default posture).
    #[serde(default)]
    review_depth: std::collections::BTreeMap<String, ReviewDepthCfg>,
    /// Review-date engine thresholds (WS3): POC review window, automatic-extension
    /// allowance, and sweep cadence. Defaults are the policy doc's 90 days / 1.
    #[serde(default)]
    review: ReviewCfg,
    /// Governance metric thresholds (WS4). The only org-specific number is the
    /// two-maintainer minimum for Wide/Critical systems.
    #[serde(default)]
    governance: GovernanceCfg,
    #[serde(default)]
    provisioning: Option<ProvisioningCfg>,
    /// Operator overlay for reviewer manifests: files named `reviewer.yaml` under
    /// here add or override the embedded reviewers (e.g. disable the built-in
    /// `llm-judge`, or add an external `webhook` reviewer). No recompile.
    #[serde(default)]
    reviewers_dir: Option<PathBuf>,
}

impl Config {
    /// Reviewer overlay dir: config value, else the `ASGARD_REVIEWERS_DIR` env var.
    fn reviewers_dir(&self) -> Option<PathBuf> {
        self.reviewers_dir.clone().or_else(|| {
            std::env::var("ASGARD_REVIEWERS_DIR")
                .ok()
                .map(PathBuf::from)
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewCfg {
    #[serde(default = "default_poc_window")]
    poc_window_days: i64,
    #[serde(default = "default_auto_extensions")]
    auto_extensions: i64,
    /// How often the review sweep runs (seconds). Absent = daily.
    #[serde(default)]
    sweep_secs: Option<u64>,
    /// How often the async code-review worker drains its queue (seconds). Absent
    /// = 15s.
    #[serde(default)]
    worker_secs: Option<u64>,
}

impl Default for ReviewCfg {
    fn default() -> Self {
        ReviewCfg {
            poc_window_days: default_poc_window(),
            auto_extensions: default_auto_extensions(),
            sweep_secs: None,
            worker_secs: None,
        }
    }
}

/// One tier's code-review depth override (see `Config::review_depth`).
#[derive(Debug, Clone, Deserialize)]
struct ReviewDepthCfg {
    /// Skip code review for this tier entirely.
    #[serde(default)]
    skip: bool,
    /// Standard ids to judge against (e.g. `["coding", "security"]`).
    #[serde(default)]
    standards: Vec<String>,
    /// Tool-loop round budget (how many `list_files`/`read_file` cycles the model
    /// gets to navigate the repo). Absent = 8.
    #[serde(default = "default_review_rounds")]
    max_rounds: usize,
}

fn default_review_rounds() -> usize {
    8
}

fn default_poc_window() -> i64 {
    90
}

fn default_auto_extensions() -> i64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
struct GovernanceCfg {
    #[serde(default = "default_maintainer_min")]
    maintainer_min: i64,
}

impl Default for GovernanceCfg {
    fn default() -> Self {
        GovernanceCfg {
            maintainer_min: default_maintainer_min(),
        }
    }
}

fn default_maintainer_min() -> i64 {
    2
}

#[derive(Debug, Clone, Deserialize)]
struct RegistrationCfg {
    #[serde(default = "default_true")]
    require_manager: bool,
    #[serde(default = "default_true")]
    require_group: bool,
}

impl Default for RegistrationCfg {
    fn default() -> Self {
        RegistrationCfg {
            require_manager: true,
            require_group: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProvisioningCfg {
    #[serde(default)]
    default_cloud: Option<String>,
    #[serde(default)]
    default_account: Option<String>,
    /// Allowed (cloud, account) targets. A request to anything else is refused.
    #[serde(default)]
    allowed: Vec<TargetCfg>,
    #[serde(default)]
    auto_approve: Option<AutoApproveCfg>,
    #[serde(default)]
    aws: Option<AwsCfg>,
    /// Operator manifest overlay: files named `service.yaml` under here add or
    /// override services on top of the embedded defaults.
    #[serde(default)]
    services_dir: Option<PathBuf>,
    #[serde(default)]
    terraform: Option<TerraformCfg>,
    #[serde(default)]
    secrets: Option<SecretsCfg>,
    /// A command the `exec` cost source runs to attribute spend (USD on stdout).
    #[serde(default)]
    exec_cost_command: Vec<String>,
    /// How often the cost-rollup routine runs (seconds). Absent = hourly.
    #[serde(default)]
    rollup_secs: Option<u64>,
    #[serde(default)]
    forecast_window_days: Option<i64>,
    #[serde(default)]
    anomaly_z: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TerraformCfg {
    #[serde(default = "default_tf_bin")]
    bin: String,
    /// Base directory the hub-supplied TF modules live in (relative manifest
    /// `module` paths resolve against this).
    modules_dir: PathBuf,
    /// Where per-resource working dirs + local state are persisted.
    #[serde(default)]
    work_dir: Option<PathBuf>,
}

fn default_tf_bin() -> String {
    "terraform".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SecretsCfg {
    /// 32-byte master key as 64 hex chars for the builtin store. Overridden by
    /// the `ASGARD_SECRET_KEY` env var. Absent = the dev key (single-binary
    /// out-of-the-box only; set a real key in production).
    #[serde(default)]
    master_key_hex: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TargetCfg {
    cloud: String,
    account: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AutoApproveCfg {
    /// classification → monthly self-service ceiling (USD). A classification
    /// absent from the map never auto-approves.
    #[serde(default)]
    ceilings: std::collections::BTreeMap<String, f64>,
}

/// AWS **cost** configuration (Cost Explorer reads). AWS *provisioning* is the
/// `terraform` connector's job — the bespoke AWS connector was removed — so this
/// block carries only what the cost sources need.
#[derive(Debug, Clone, Default, Deserialize)]
struct AwsCfg {
    region: String,
    #[serde(default)]
    profile: Option<String>,
    /// Whether live provisioning is armed elsewhere; here it only supplies the
    /// default for `cost_explorer` when that isn't set explicitly.
    #[serde(default)]
    execute: bool,
    /// Live Cost Explorer reads (non-destructive). Defaults to `execute` when
    /// unset. Enable to get real cost without arming live provisioning.
    #[serde(default)]
    cost_explorer: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceCfg {
    provider: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Restore default SIGPIPE so `asgard … | head` terminates quietly like any
    // Unix tool instead of panicking on a broken-pipe write.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Load a local .env (if present) before anything reads the environment, so
    // provider keys and service master credentials (OpenAI, AWS, Auth0, …) can
    // live in one gitignored file. Real env vars always win over .env entries.
    let _ = dotenvy::dotenv();

    let Cli {
        database_url,
        url,
        pat,
        profile,
        output,
        command,
    } = Cli::parse();

    // Logs go to stderr (never stdout, which carries CLI results). Server
    // commands log at info; CLI commands stay quiet so client/transport chatter
    // doesn't drown the output. RUST_LOG overrides either default.
    let default_filter = if matches!(command, Cmd::Serve { .. } | Cmd::Mcp { .. }) {
        "info"
    } else {
        "warn"
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    match command {
        // --- in-process server + offline commands (no PAT/URL) ---------------
        Cmd::Serve { bind, config } => serve(&database_url, &bind, config).await?,
        Cmd::Mcp { config } => run_mcp(&database_url, config).await?,
        Cmd::Init => {
            let p = asgard_cli::init_config(Path::new("."))?;
            println!("wrote {}", p.display());
        }
        Cmd::Validate { path } => {
            let report = asgard_cli::validate_manifest(&path)?;
            println!("{report}");
        }
        Cmd::Agent {
            cmd: AgentCmd::New { template, name },
        } => {
            let dir = PathBuf::from(&name);
            let written = asgard_cli::agent_new(&template, &dir)?;
            println!("scaffolded {} files into {}/", written.len(), dir.display());
        }
        Cmd::Completions { shell } => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut cmd, "asgard", &mut std::io::stdout());
        }
        Cmd::Login { no_default } => {
            cmd_login(url.clone(), pat.clone(), profile.clone(), no_default).await?
        }

        // --- PAT-authed MCP client commands ----------------------------------
        Cmd::Tools => {
            let r = conn(&url, &pat, &profile, &output)?;
            cmd_tools(&r).await;
        }
        Cmd::Call { tool, json, arg } => {
            let r = conn(&url, &pat, &profile, &output)?;
            let args = asgard_cli::mcp::args_from(json, &arg)?;
            run_tool(&r, &tool, args, Shape::Auto).await;
        }
        Cmd::Catalog { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                CatalogCmd::Search { kind, query } => {
                    let mut m = Map::new();
                    opt(&mut m, "kind", kind);
                    opt(&mut m, "query", query);
                    run_tool(&r, "catalog_search", m, Shape::Auto).await;
                }
                CatalogCmd::Get {
                    kind,
                    namespace,
                    name,
                } => {
                    let mut m = Map::new();
                    m.insert("kind".into(), json!(kind));
                    m.insert("name".into(), json!(name));
                    opt(&mut m, "namespace", namespace);
                    run_tool(&r, "catalog_get", m, Shape::Auto).await;
                }
                CatalogCmd::Services => {
                    run_tool(&r, "list_services", Map::new(), Shape::Auto).await
                }
                CatalogCmd::Service { id } => {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(id));
                    run_tool(&r, "get_service", m, Shape::Auto).await;
                }
                CatalogCmd::Groups => run_tool(&r, "list_groups", Map::new(), Shape::Auto).await,
            }
        }
        Cmd::Project { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                ProjectCmd::Ls => {
                    run_tool(
                        &r,
                        "list_projects",
                        Map::new(),
                        Shape::Rows(vec![
                            "project_id",
                            "name",
                            "owner",
                            "manager",
                            "group",
                            "classification",
                            "lifecycle",
                            "budget_usd",
                            "spent_usd",
                        ]),
                    )
                    .await
                }
                ProjectCmd::Register {
                    name,
                    owner,
                    manager,
                    group,
                    classification,
                    data_class,
                    budget_usd,
                    description,
                } => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(name));
                    if !owner.is_empty() {
                        m.insert("owner_email".into(), json!(owner));
                    }
                    if !manager.is_empty() {
                        m.insert("manager_email".into(), json!(manager));
                    }
                    if !group.is_empty() {
                        m.insert("group".into(), json!(group));
                    }
                    opt(&mut m, "classification", classification);
                    opt(&mut m, "data_class", data_class);
                    opt(&mut m, "budget_usd", budget_usd);
                    opt(&mut m, "description", description);
                    run_tool(&r, "register_project", m, Shape::Auto).await;
                }
                ProjectCmd::Update {
                    project_id,
                    name,
                    description,
                    budget_usd,
                } => {
                    let mut m = Map::new();
                    m.insert("project_id".into(), json!(project_id));
                    opt(&mut m, "name", name);
                    opt(&mut m, "description", description);
                    opt(&mut m, "budget_usd", budget_usd);
                    run_tool(&r, "update_project", m, Shape::Auto).await;
                }
                ProjectCmd::Get { project_id } => {
                    run_tool(
                        &r,
                        "project_get",
                        one("project_id", project_id),
                        Shape::Auto,
                    )
                    .await
                }
                ProjectCmd::State { project_id } => {
                    run_tool(
                        &r,
                        "project_state",
                        one("project_id", project_id),
                        Shape::Auto,
                    )
                    .await
                }
                ProjectCmd::Credential { project_id } => {
                    run_tool(
                        &r,
                        "gateway_credential",
                        one("project_id", project_id),
                        Shape::Auto,
                    )
                    .await
                }
                ProjectCmd::Promotion { project_id } => {
                    run_tool(
                        &r,
                        "promotion_status",
                        one("project_id", project_id),
                        Shape::Auto,
                    )
                    .await
                }
                ProjectCmd::Promote { project_id, target } => {
                    let mut m = Map::new();
                    m.insert("project_id".into(), json!(project_id));
                    m.insert("target".into(), json!(target));
                    run_tool(&r, "request_promotion", m, Shape::Auto).await;
                }
                ProjectCmd::Escalate { request_id } => {
                    run_tool(
                        &r,
                        "escalate_promotion",
                        one("request_id", request_id),
                        Shape::Auto,
                    )
                    .await
                }
            }
        }
        Cmd::Standards { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                StandardsCmd::Ls => run_tool(&r, "list_standards", Map::new(), Shape::Auto).await,
                StandardsCmd::Get { id } => {
                    run_tool(&r, "get_standards", one("id", id), Shape::Auto).await
                }
            }
        }
        Cmd::Guidance { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                GuidanceCmd::Ls => run_tool(&r, "guidance_list", Map::new(), Shape::Auto).await,
                GuidanceCmd::Get { slug } => {
                    run_tool(&r, "guidance_get", one("slug", slug), Shape::Auto).await
                }
                GuidanceCmd::Put {
                    title,
                    body,
                    slug,
                    summary,
                    category,
                    tags,
                } => {
                    let mut m = Map::new();
                    m.insert("title".into(), json!(title));
                    m.insert("body".into(), json!(body));
                    opt(&mut m, "slug", slug);
                    opt(&mut m, "summary", summary);
                    opt(&mut m, "category", category);
                    if !tags.is_empty() {
                        m.insert("tags".into(), json!(tags));
                    }
                    run_tool(&r, "guidance_put", m, Shape::Auto).await;
                }
            }
        }
        Cmd::Recipe { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                RecipeCmd::Ls => run_tool(&r, "recipe_list", Map::new(), Shape::Auto).await,
                RecipeCmd::Get { slug } => {
                    run_tool(&r, "recipe_get", one("slug", slug), Shape::Auto).await
                }
                RecipeCmd::Put {
                    name,
                    body,
                    slug,
                    summary,
                    spec,
                    tags,
                } => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(name));
                    m.insert("body".into(), json!(body));
                    opt(&mut m, "slug", slug);
                    opt(&mut m, "summary", summary);
                    if let Some(s) = spec {
                        m.insert("spec".into(), parse_json(&s)?);
                    }
                    if !tags.is_empty() {
                        m.insert("tags".into(), json!(tags));
                    }
                    run_tool(&r, "recipe_put", m, Shape::Auto).await;
                }
            }
        }
        Cmd::McpCatalog { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                McpCatalogCmd::Ls => {
                    run_tool(&r, "mcp_catalog_list", Map::new(), Shape::Auto).await
                }
                McpCatalogCmd::Get { id } => {
                    run_tool(&r, "mcp_catalog_get", one("id", id), Shape::Auto).await
                }
                McpCatalogCmd::Publish {
                    name,
                    id,
                    summary,
                    readme,
                    install,
                    repository,
                    homepage,
                    version,
                    tags,
                } => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(name));
                    opt(&mut m, "id", id);
                    opt(&mut m, "summary", summary);
                    opt(&mut m, "readme", readme);
                    if let Some(s) = install {
                        m.insert("install".into(), parse_json(&s)?);
                    }
                    opt(&mut m, "repository", repository);
                    opt(&mut m, "homepage", homepage);
                    opt(&mut m, "version", version);
                    if !tags.is_empty() {
                        m.insert("tags".into(), json!(tags));
                    }
                    run_tool(&r, "mcp_catalog_publish", m, Shape::Auto).await;
                }
                McpCatalogCmd::SetState { id, state } => {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(id));
                    m.insert("state".into(), json!(state));
                    run_tool(&r, "mcp_catalog_set_state", m, Shape::Auto).await;
                }
            }
        }
        Cmd::Skills { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                SkillsCmd::Ls => run_tool(&r, "skills_catalog_list", Map::new(), Shape::Auto).await,
                SkillsCmd::Get { id } => {
                    run_tool(&r, "skills_catalog_get", one("id", id), Shape::Auto).await
                }
                SkillsCmd::Publish {
                    id,
                    dir,
                    bundle,
                    name,
                    summary,
                    readme,
                    runtime,
                    repository,
                    homepage,
                    version,
                    tags,
                } => {
                    let mut m = Map::new();
                    m.insert("bundle".into(), bundle_arg(dir, bundle)?);
                    opt(&mut m, "id", id);
                    for (k, v) in [
                        ("name", name),
                        ("summary", summary),
                        ("readme", readme),
                        ("runtime", runtime),
                        ("repository", repository),
                        ("homepage", homepage),
                        ("version", version),
                    ] {
                        if !v.is_empty() {
                            m.insert(k.into(), json!(v));
                        }
                    }
                    if !tags.is_empty() {
                        m.insert("tags".into(), json!(tags));
                    }
                    run_tool(&r, "skills_catalog_publish", m, Shape::Auto).await;
                }
                SkillsCmd::SetState { id, state } => {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(id));
                    m.insert("state".into(), json!(state));
                    run_tool(&r, "skills_catalog_set_state", m, Shape::Auto).await;
                }
                SkillsCmd::Export { id, runtime, out } => {
                    cmd_skills_export(&r, id, runtime, out).await
                }
                SkillsCmd::Install {
                    id,
                    dest,
                    dest_dir,
                    dry_run,
                    force,
                } => cmd_skills_install(&r, id, dest, dest_dir, dry_run, force).await,
            }
        }
        Cmd::Cost { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                CostCmd::Report { by, since, until } => {
                    let mut m = Map::new();
                    opt(&mut m, "by", by);
                    opt(&mut m, "since", since);
                    opt(&mut m, "until", until);
                    run_tool(&r, "cost_report", m, Shape::Auto).await;
                }
                CostCmd::Project {
                    project_id,
                    start,
                    end,
                } => {
                    let mut m = Map::new();
                    m.insert("project_id".into(), json!(project_id));
                    opt(&mut m, "start", start);
                    opt(&mut m, "end", end);
                    run_tool(&r, "project_cost", m, Shape::Auto).await;
                }
                CostCmd::Series {
                    project,
                    from,
                    until,
                } => {
                    let mut m = Map::new();
                    m.insert("project".into(), json!(project));
                    opt(&mut m, "from", from);
                    opt(&mut m, "until", until);
                    run_tool(&r, "cost_series", m, Shape::Auto).await;
                }
                CostCmd::By { by, from, until } => {
                    let mut m = Map::new();
                    opt(&mut m, "by", by);
                    opt(&mut m, "from", from);
                    opt(&mut m, "until", until);
                    run_tool(&r, "cost_by", m, Shape::Auto).await;
                }
                CostCmd::Forecast { project_id } => {
                    run_tool(
                        &r,
                        "cost_forecast",
                        one("project_id", project_id),
                        Shape::Auto,
                    )
                    .await
                }
                CostCmd::Anomalies { project, limit } => {
                    let mut m = Map::new();
                    opt(&mut m, "project", project);
                    opt(&mut m, "limit", limit);
                    run_tool(&r, "cost_anomalies", m, Shape::Auto).await;
                }
                CostCmd::Tree { as_of } => {
                    let mut m = Map::new();
                    opt(&mut m, "as_of", as_of);
                    run_tool(&r, "cost_tree", m, Shape::Auto).await;
                }
                CostCmd::Movers { as_of, top } => {
                    let mut m = Map::new();
                    opt(&mut m, "as_of", as_of);
                    opt(&mut m, "top", top);
                    run_tool(&r, "cost_movers", m, Shape::Auto).await;
                }
            }
        }
        Cmd::Governance => {
            let r = conn(&url, &pat, &profile, &output)?;
            run_tool(&r, "governance_metrics", Map::new(), Shape::Auto).await;
        }
        Cmd::Resource { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                ResourceCmd::Request {
                    resource_type,
                    name,
                    project,
                    spec,
                } => {
                    let mut m = Map::new();
                    m.insert("resource_type".into(), json!(resource_type));
                    m.insert("name".into(), json!(name));
                    opt(&mut m, "project_id", project);
                    if let Some(s) = spec {
                        m.insert("spec".into(), parse_json(&s)?);
                    }
                    run_tool(&r, "request_resource", m, Shape::Auto).await;
                }
                ResourceCmd::Grant {
                    consumer,
                    target,
                    project,
                    level,
                } => {
                    let mut m = Map::new();
                    m.insert("consumer_resource_id".into(), json!(consumer));
                    m.insert("target_resource_id".into(), json!(target));
                    opt(&mut m, "project_id", project);
                    opt(&mut m, "level", level);
                    run_tool(&r, "request_grant", m, Shape::Auto).await;
                }
                ResourceCmd::Ls { project, state } => {
                    let mut m = Map::new();
                    opt(&mut m, "project_id", project);
                    opt(&mut m, "state", state);
                    run_tool(&r, "list_resources", m, Shape::Auto).await;
                }
                ResourceCmd::Get { resource_id } => {
                    run_tool(
                        &r,
                        "get_resource",
                        one("resource_id", resource_id),
                        Shape::Auto,
                    )
                    .await
                }
                ResourceCmd::Runs { resource_id } => {
                    run_tool(
                        &r,
                        "resource_runs",
                        one("resource_id", resource_id),
                        Shape::Auto,
                    )
                    .await
                }
                ResourceCmd::Retry { resource_id } => {
                    run_tool(
                        &r,
                        "retry_resource",
                        one("resource_id", resource_id),
                        Shape::Auto,
                    )
                    .await
                }
                ResourceCmd::Deprovision { resource_id } => {
                    run_tool(
                        &r,
                        "deprovision_resource",
                        one("resource_id", resource_id),
                        Shape::Auto,
                    )
                    .await
                }
                ResourceCmd::DeployImage {
                    resource_id,
                    image,
                    project,
                } => {
                    let mut m = Map::new();
                    m.insert("resource_id".into(), json!(resource_id));
                    m.insert("image".into(), json!(image));
                    opt(&mut m, "project_id", project);
                    run_tool(&r, "deploy_image", m, Shape::Auto).await;
                }
            }
        }
        Cmd::Secret { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                SecretCmd::Get { name, project } => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(name));
                    opt(&mut m, "project_id", project);
                    run_tool(&r, "get_secret", m, Shape::Auto).await;
                }
                SecretCmd::Rotate { name, project } => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(name));
                    opt(&mut m, "project_id", project);
                    run_tool(&r, "rotate_secret", m, Shape::Auto).await;
                }
                SecretCmd::Ls { project } => {
                    let mut m = Map::new();
                    opt(&mut m, "project_id", project);
                    run_tool(&r, "list_secrets", m, Shape::Auto).await;
                }
            }
        }
        Cmd::Seed { cmd } => {
            let r = conn(&url, &pat, &profile, &output)?;
            match cmd {
                SeedCmd::Ls => run_tool(&r, "seed_list", Map::new(), Shape::Auto).await,
                SeedCmd::Plan {
                    languages,
                    task,
                    tier,
                } => {
                    let mut m = Map::new();
                    if !languages.is_empty() {
                        m.insert("languages".into(), json!(languages));
                    }
                    opt(&mut m, "task", task);
                    opt(&mut m, "tier", tier);
                    run_tool(&r, "seed_plan", m, Shape::Auto).await;
                }
                SeedCmd::Get { id } => run_tool(&r, "seed_get", one("id", id), Shape::Auto).await,
                SeedCmd::Apply {
                    languages,
                    task,
                    tier,
                    write,
                    force,
                    dir,
                } => cmd_apply_seed(&r, languages, task, tier, write, force, dir).await,
            }
        }
        Cmd::Bootstrap {
            languages,
            task,
            tier,
            write,
            force,
            dir,
        } => {
            let r = conn(&url, &pat, &profile, &output)?;
            cmd_apply_seed(&r, languages, task, tier, write, force, dir).await;
        }
        Cmd::Chat {
            project,
            model,
            message,
            max_tokens,
            temperature,
            data_class,
        } => {
            let r = conn(&url, &pat, &profile, &output)?;
            let prof = profile.clone().unwrap_or_else(|| "default".to_string());
            cmd_chat(
                &r,
                prof,
                project,
                model,
                message,
                max_tokens,
                temperature,
                data_class,
            )
            .await;
        }
    }
    Ok(())
}

// --- CLI dispatch helpers ----------------------------------------------------

/// Resolve connection + output settings (flag/env, then profile, then defaults).
fn conn(
    url: &Option<String>,
    pat: &Option<String>,
    profile: &Option<String>,
    output: &Option<String>,
) -> anyhow::Result<Resolved> {
    Ok(asgard_cli::config::load().resolve(
        url.clone(),
        pat.clone(),
        profile.clone(),
        output.clone(),
    )?)
}

/// A single-key argument object (the common case).
fn one(key: &str, val: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(key.to_string(), json!(val));
    m
}

/// Insert `key` only when the option is present.
fn opt(m: &mut Map<String, Value>, key: &str, v: Option<impl Into<Value>>) {
    if let Some(x) = v {
        m.insert(key.to_string(), x.into());
    }
}

fn parse_json(s: &str) -> anyhow::Result<Value> {
    serde_json::from_str(s).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))
}

/// The PAT, or a fail-fast exit-3 with actionable guidance.
fn require_pat(r: &Resolved) -> String {
    match &r.pat {
        Some(p) => p.clone(),
        None => {
            eprintln!(
                "error: no PAT configured — pass --pat, set ASGARD_PAT, or run `asgard login`"
            );
            std::process::exit(3);
        }
    }
}

/// Render a tool result and exit with a stable code: 0 ok, 2 tool error, else the
/// error's own code (3 auth / 1 transport).
fn emit(
    out: Result<asgard_cli::mcp::ToolOutput, asgard_cli::CliError>,
    shape: Shape,
    output: Output,
) -> ! {
    match out {
        Ok(t) if t.is_error => {
            eprintln!("{}", t.raw_text);
            std::process::exit(2);
        }
        Ok(t) => {
            println!("{}", render(&t.value, shape, output));
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

/// Call one tool and emit the result (diverges).
async fn run_tool(r: &Resolved, tool: &str, args: Map<String, Value>, shape: Shape) {
    let pat = require_pat(r);
    let out = McpClient::new(&r.url, pat).call(tool, args).await;
    emit(out, shape, r.output);
}

async fn cmd_tools(r: &Resolved) {
    let pat = require_pat(r);
    match McpClient::new(&r.url, pat).tools().await {
        Ok(ts) => {
            let val = Value::Array(
                ts.iter()
                    .map(|t| json!({ "name": t.name, "description": t.description }))
                    .collect(),
            );
            println!(
                "{}",
                render(&val, Shape::Rows(vec!["name", "description"]), r.output)
            );
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

fn read_pat() -> anyhow::Result<String> {
    use std::io::Write;
    eprint!("Asgard PAT (asg_pat_…): ");
    std::io::stderr().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim().to_string();
    anyhow::ensure!(!s.is_empty(), "no PAT entered");
    Ok(s)
}

async fn cmd_login(
    url: Option<String>,
    pat: Option<String>,
    profile: Option<String>,
    no_default: bool,
) -> anyhow::Result<()> {
    let prof = profile.unwrap_or_else(|| "default".to_string());
    let pat = match pat {
        Some(p) => p,
        None => read_pat()?,
    };
    if let Some(u) = &url {
        if let Err(e) = McpClient::new(u, pat.clone()).tools().await {
            eprintln!("error: that PAT did not validate against {u}: {e}");
            std::process::exit(3);
        }
    }
    let path = asgard_cli::config::save_login(&prof, url.as_deref(), &pat, !no_default)?;
    println!("saved profile '{prof}' → {}", path.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_chat(
    r: &Resolved,
    profile: String,
    project: String,
    model: String,
    message: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    data_class: Option<String>,
) {
    let pat = require_pat(r);
    let req = asgard_cli::chat::ChatRequest {
        url: &r.url,
        pat: &pat,
        profile: &profile,
        project: &project,
        model,
        message,
        max_tokens,
        temperature,
        data_class,
    };
    match asgard_cli::chat::chat(req).await {
        Ok(v) => println!("{}", render(&v, Shape::Auto, r.output)),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

/// Call a tool and return its JSON value, exiting on transport/tool error (the
/// shape for commands that post-process the result locally — e.g. write files).
async fn call_value(r: &Resolved, tool: &str, args: Map<String, Value>) -> Value {
    let pat = require_pat(r);
    let out = match McpClient::new(&r.url, pat).call(tool, args).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    };
    if out.is_error {
        eprintln!("{}", out.raw_text);
        std::process::exit(2);
    }
    out.value
}

/// Print a materialization result (one line per file); `note` is an optional
/// trailing message (e.g. the dry-run hint).
fn emit_writes(
    res: Result<Vec<(PathBuf, asgard_cli::seed::Action)>, asgard_cli::CliError>,
    note: Option<&str>,
) {
    match res {
        Ok(results) => {
            for (p, a) in &results {
                println!("{}\t{}", a.label(), p.display());
            }
            if let Some(n) = note {
                eprintln!("\n{n}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

/// Resolve the publish bundle from exactly one of --dir (walk + base64) or
/// --bundle (raw JSON array; "-" reads stdin). clap's `conflicts_with` already
/// rejects passing both.
fn bundle_arg(dir: Option<PathBuf>, bundle: Option<String>) -> anyhow::Result<Value> {
    match (dir, bundle) {
        (Some(d), _) => Ok(asgard_cli::seed::dir_to_bundle(&d)?),
        (None, Some(b)) => {
            let raw = if b == "-" {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                b
            };
            parse_json(&raw)
        }
        (None, None) => {
            anyhow::bail!("provide the skill files with --dir <path> or --bundle <json|->")
        }
    }
}

/// Expand a leading `~/` to `$HOME` — the install tool returns a display path.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

async fn cmd_apply_seed(
    r: &Resolved,
    languages: Vec<String>,
    task: Option<String>,
    tier: Option<String>,
    write: bool,
    force: bool,
    dir: Option<PathBuf>,
) {
    let mut m = Map::new();
    if !languages.is_empty() {
        m.insert("languages".into(), json!(languages));
    }
    opt(&mut m, "task", task);
    opt(&mut m, "tier", tier);
    let value = call_value(r, "bootstrap", m).await;
    let dest = dir.unwrap_or_else(|| PathBuf::from("."));
    let note = (!write).then_some("dry run — pass --write to create these files");
    emit_writes(asgard_cli::seed::apply(&value, &dest, write, force), note);
}

async fn cmd_skills_export(
    r: &Resolved,
    id: String,
    runtime: Option<String>,
    out: Option<PathBuf>,
) {
    let mut m = Map::new();
    m.insert("id".into(), json!(id));
    opt(&mut m, "runtime", runtime);
    let value = call_value(r, "skills_catalog_export", m).await;
    let dest = out.unwrap_or_else(|| PathBuf::from("."));
    emit_writes(
        asgard_cli::seed::apply_b64(&value, &dest, true, false),
        None,
    );
}

async fn cmd_skills_install(
    r: &Resolved,
    id: String,
    dest: Option<String>,
    dest_dir: Option<PathBuf>,
    dry_run: bool,
    force: bool,
) {
    let mut m = Map::new();
    m.insert("id".into(), json!(id));
    opt(&mut m, "dest", dest);
    let value = call_value(r, "skills_catalog_install", m).await;
    let target = match dest_dir {
        Some(d) => d,
        None => expand_tilde(value.get("dir").and_then(Value::as_str).unwrap_or(".")),
    };
    let note = dry_run.then_some("dry run — re-run without --dry-run to write");
    emit_writes(
        asgard_cli::seed::apply_install(&value, &target, !dry_run, force),
        note,
    );
}

struct Core {
    db: Db,
    catalog: CatalogRepo,
    schemas: SchemaRegistry,
    policy: Arc<dyn PolicyEngine>,
    identity: IdentityService,
    gateway_repo: GatewayRepo,
}

async fn build_core(database_url: &str) -> anyhow::Result<Core> {
    let db = Db::connect(database_url).await?;
    db.migrate().await?;
    let catalog = CatalogRepo::new(db.clone());
    let schemas = SchemaRegistry::embedded()?;
    let policy: Arc<dyn PolicyEngine> = Arc::new(CedarEngine::new()?);
    let identity = IdentityService::new(db.clone());
    let gateway_repo = GatewayRepo::new(db.clone());
    Ok(Core {
        db,
        catalog,
        schemas,
        policy,
        identity,
        gateway_repo,
    })
}

async fn serve(database_url: &str, bind: &str, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = load_config(config_path);
    let core = build_core(database_url).await?;
    let git_token = std::env::var("ASGARD_GIT_TOKEN").ok();

    // Initial reconcile so entities appear promptly.
    reconcile_all(
        &core.catalog,
        &core.schemas,
        &config.sources,
        git_token.clone(),
    )
    .await;

    // Publish the enterprise standards as discoverable catalog entities.
    if let Err(e) = asgard_catalog::standards::seed(&core.catalog).await {
        tracing::warn!("failed to seed standards: {e}");
    }

    // Inference backends are manifest-driven service modules: build the gateway's
    // providers + models from the enabled inference modules in the catalog (active
    // when their credentials are present), plus the always-on mock for offline use.
    let service_catalog = load_service_catalog(&config);
    let (providers, inf_models) = build_inference(&service_catalog);
    let cost_qa_model = inf_models
        .first()
        .map(|m| m.model_ref.clone())
        .unwrap_or_else(|| "model:default/mock".to_string());
    // The review gate needs a real model to judge anything. It's enabled only when a
    // real inference backend is active (an OpenAI/Anthropic key, or an enabled
    // openai-compatible gateway module like LiteLLM) — the offline mock does not
    // count. `ASGARD_REVIEW_ALLOW_MOCK=1` forces it on for dev/tests (mock judge).
    let has_real_llm = !inf_models.is_empty();
    let mut model_registry = ModelRegistry::from_catalog(&core.catalog).await?;
    model_registry.insert(default_mock_model());
    for m in inf_models {
        model_registry.insert(m);
    }

    let gateway = Arc::new(Gateway::new(
        core.gateway_repo.clone(),
        core.policy.clone(),
        model_registry,
        providers,
        guardrail_mode(),
    ));
    let workflow = Arc::new(WorkflowEngine::new(core.db.clone(), core.policy.clone()));
    let lease_ttl = config.lease_ttl_secs.unwrap_or(600) as i64;
    let leases = Leases::new(core.db.clone(), asgard_storage::new_uid());
    let mut provision =
        build_provision(core.db.clone(), &config, Some((leases.clone(), lease_ttl)));
    provision.set_workflow(workflow.clone());
    if let Some(secs) = config.provision_wait_secs {
        provision.set_wait_budget_secs(secs);
    }
    if let Some(n) = config.provision_max_retries {
        provision.set_max_retries(n);
    }

    // A platform-owned system project + gateway key so the dashboard's cost Q&A and
    // the built-in review judge work without a human pasting a key. Spend is
    // attributed and governed like any other project's calls.
    let _ = core
        .gateway_repo
        .ensure_project("proj-asgard-system", 0.0, "internal")
        .await;
    let system_cost_key = core
        .gateway_repo
        .mint_key("proj-asgard-system", Some("dashboard"))
        .await
        .ok()
        .map(|k| k.plaintext);

    let review_enabled = has_real_llm || review_allow_mock();
    let mut registry = ProjectRegistry::new(
        core.db.clone(),
        core.gateway_repo.clone(),
        core.catalog.clone(),
        build_allowlist(&config),
        build_registration_policy(&config),
    )
    .with_requirements(build_requirements(&config))
    .with_review_config(build_review_config(&config))
    .with_governance_config(build_governance_config(&config));

    // The machine-review panel: built-in `llm-judge` (in-process, system-key) plus
    // external `webhook` delegation, dispatched by manifest `kind`. Attached only
    // when a real LLM is reachable; otherwise promotion stays a pure presence check.
    if review_enabled {
        let reviewer_catalog = ReviewerCatalog::load(config.reviewers_dir().as_deref())
            .unwrap_or_else(|e| {
                tracing::warn!("reviewer catalog load failed: {e}; using embedded defaults");
                ReviewerCatalog::embedded().unwrap_or_default()
            });
        let mut reviewer_registry = ReviewerRegistry::new();
        reviewer_registry.register(
            "llm-judge",
            Arc::new(LlmJudge::new(gateway.clone(), system_cost_key.clone())),
        );
        reviewer_registry.register("webhook", Arc::new(WebhookReviewer::new()));
        // The deep async reviewer: reads the repo over Asgard's git token and judges
        // it against the org standards, depth per target tier. Runs in the worker.
        reviewer_registry.register(
            "code-review",
            Arc::new(CodeReview::new(
                gateway.clone(),
                system_cost_key.clone(),
                Arc::new(RegistryStandards::new(core.db.clone())),
                build_review_depth(&config),
            )),
        );
        let review_service = Arc::new(ReviewService::new(
            reviewer_catalog,
            reviewer_registry,
            cost_qa_model.clone(),
        ));
        registry = registry.with_reviewer_panel(review_service);
        tracing::info!("review gate enabled (model: {cost_qa_model})");
    } else {
        tracing::info!(
            "review gate DISABLED: no LLM access — set an OpenAI/Anthropic key or enable an \
             LLM gateway module (LiteLLM/etc.) to turn it on; promotion uses the presence check"
        );
    }
    maybe_seed_admin(&core.identity).await;
    if let Err(e) = registry.seed_knowledge().await {
        tracing::warn!("seeding starter guidance/recipes failed: {e}");
    }

    let oidc = build_oidc();
    let state = AppState::new(
        core.db.clone(),
        core.catalog.clone(),
        core.schemas.clone(),
        gateway,
        workflow,
        registry,
        provision,
        core.identity.clone(),
    )
    .with_system_name(std::env::var("ASGARD_SYSTEM_NAME").unwrap_or_default())
    .with_system_cost_key(system_cost_key)
    .with_cost_qa_model(cost_qa_model)
    .with_oidc(oidc.clone())
    .with_oidc_roles(build_oidc_roles())
    .with_disable_local_login(resolve_disable_local_login(oidc.is_some()))
    .with_dev_insecure(resolve_dev_insecure(bind))
    .with_force_https(matches!(
        std::env::var("ASGARD_FORCE_HTTPS").as_deref(),
        Ok("1") | Ok("true")
    ));

    // Periodic reconcile keeps the catalog in sync (deletes propagate).
    if !config.sources.is_empty() {
        let secs = config.reconcile_secs.unwrap_or(120);
        let (cat, reg, srcs, tok) = (
            core.catalog.clone(),
            core.schemas.clone(),
            config.sources.clone(),
            git_token.clone(),
        );
        let lease = leases.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                if lease
                    .try_acquire("loop:reconcile", lease_ttl)
                    .await
                    .unwrap_or(false)
                {
                    reconcile_all(&cat, &reg, &srcs, tok.clone()).await;
                    let _ = lease.release("loop:reconcile").await;
                }
            }
        });
    }

    // Periodic secret-rotation sweep: auto-rotate secrets past their interval.
    {
        let prov = state.provision.clone();
        let secs = config.reconcile_secs.unwrap_or(120).max(60);
        let lease = leases.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                if lease
                    .try_acquire("loop:rotation", lease_ttl)
                    .await
                    .unwrap_or(false)
                {
                    match prov.rotate_due_secrets().await {
                        Ok(n) if n > 0 => tracing::info!("rotated {n} due secret(s)"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("secret rotation sweep failed: {e}"),
                    }
                    let _ = lease.release("loop:rotation").await;
                }
            }
        });
    }

    // Periodic cost rollup: fan every project's cost sources into the persisted
    // daily rollup, then refit forecasts and flag anomalies. Idempotent per day.
    {
        let prov = state.provision.clone();
        let reg = state.registry.clone();
        let secs = config
            .provisioning
            .as_ref()
            .and_then(|p| p.rollup_secs)
            .unwrap_or(3600)
            .max(1);
        let lease = leases.clone();
        tokio::spawn(async move {
            loop {
                if lease
                    .try_acquire("loop:rollup", lease_ttl)
                    .await
                    .unwrap_or(false)
                {
                    let day = chrono::Utc::now()
                        .date_naive()
                        .format("%Y-%m-%d")
                        .to_string();
                    match prov.roll_up_costs(&reg, &day).await {
                        Ok(s) => tracing::info!(
                            "cost rollup {}: {} project(s), {} row(s), {} forecast(s), {} anomaly(ies)",
                            s.day,
                            s.projects,
                            s.rows,
                            s.forecasts,
                            s.anomalies
                        ),
                        Err(e) => tracing::warn!("cost rollup failed: {e}"),
                    }
                    let _ = lease.release("loop:rollup").await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
        });
    }

    // Periodic review-date sweep: flag projects past their review deadline and
    // lapsed stack exceptions (WS3). Idempotent; flags + audits, blocks nothing.
    {
        let reg = state.registry.clone();
        let secs = config.review.sweep_secs.unwrap_or(86_400).max(1);
        let lease = leases.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                if lease
                    .try_acquire("loop:review", lease_ttl)
                    .await
                    .unwrap_or(false)
                {
                    match reg.sweep("system").await {
                        Ok(s) => tracing::info!(
                            "review sweep: {} checked, {} newly expired, {} lapsed exception(s)",
                            s.checked,
                            s.newly_expired.len(),
                            s.expired_exceptions.len()
                        ),
                        Err(e) => tracing::warn!("review sweep failed: {e}"),
                    }
                    let _ = lease.release("loop:review").await;
                }
            }
        });
    }

    // Async code-review worker: drain the `review_jobs` queue — reclaim crashed
    // leases, run each pending promotion's reviewer panel, and finalize (approve /
    // request / flag, or fail closed). Crash-safe (state in the DB); runs even when
    // review is disabled so any leftover `Reviewing` promotion still drains.
    {
        let reg = state.registry.clone();
        let wf = state.workflow.clone();
        let secs = config.review.worker_secs.unwrap_or(15).max(1);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                match reg.drain_reviews(&wf).await {
                    Ok(n) if n > 0 => tracing::info!("review worker: finalized {n} promotion(s)"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("review worker pass failed: {e}"),
                }
            }
        });
    }

    // Periodic provisioning reconciler: re-drive orphaned/stale apply + destroy
    // work and enqueue approved-but-recordless requests, so a dropped call, crash,
    // or redeploy can't leave a resource un-provisioned or untracked. Work-then-
    // sleep so leftover work heals on startup. Leader-leased to one replica.
    {
        let prov = state.provision.clone();
        let reg = state.registry.clone();
        let wf = state.workflow.clone();
        let secs = config.provision_reconcile_secs.unwrap_or(60).max(5);
        let lease = leases.clone();
        tokio::spawn(async move {
            loop {
                if lease
                    .try_acquire("loop:provision-reconcile", lease_ttl)
                    .await
                    .unwrap_or(false)
                {
                    match prov.reconcile(wf.as_ref(), &reg).await {
                        Ok(n) if n > 0 => {
                            tracing::info!("provision reconcile: drove {n} work item(s)")
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("provision reconcile failed: {e}"),
                    }
                    let _ = lease.release("loop:provision-reconcile").await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
        });
    }

    // Mount the MCP server (Streamable HTTP at /mcp, gated by project virtual
    // key) alongside the REST/GraphQL/UI surface — same process, same port.
    let mcp_router = asgard_mcp::http_router(
        state.catalog.clone(),
        state.gateway.clone(),
        state.registry.clone(),
        state.workflow.clone(),
        state.provision.clone(),
        core.gateway_repo.clone(),
        state.identity.clone(),
    );
    let system_name = state.system_name.clone();
    let app = asgard_api::router(state)
        .merge(mcp_router)
        .fallback(move |uri: Uri| {
            let system_name = system_name.clone();
            async move { static_handler(uri, system_name).await }
        });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(
        "asgard on http://{bind}  (UI at /, REST at /api, GraphQL at /graphql, MCP at /mcp)"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when the process is asked to stop (Ctrl-C or SIGTERM), so in-flight
/// requests drain instead of being cut. The background loops exit with the
/// process; on a single replica that is sufficient.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}

async fn run_mcp(database_url: &str, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = load_config(config_path);
    let core = build_core(database_url).await?;
    let _ = asgard_catalog::standards::seed(&core.catalog).await;
    let service_catalog = load_service_catalog(&config);
    let (providers, inf_models) = build_inference(&service_catalog);
    let mut model_registry = ModelRegistry::from_catalog(&core.catalog).await?;
    model_registry.insert(default_mock_model());
    for m in inf_models {
        model_registry.insert(m);
    }
    let gateway = Arc::new(Gateway::new(
        core.gateway_repo.clone(),
        core.policy.clone(),
        model_registry,
        providers,
        guardrail_mode(),
    ));
    let workflow = Arc::new(WorkflowEngine::new(core.db.clone(), core.policy.clone()));
    let registry = ProjectRegistry::new(
        core.db.clone(),
        core.gateway_repo.clone(),
        core.catalog.clone(),
        build_allowlist(&config),
        build_registration_policy(&config),
    )
    .with_requirements(build_requirements(&config))
    .with_review_config(build_review_config(&config))
    .with_governance_config(build_governance_config(&config));
    let mut provision = build_provision(core.db.clone(), &config, None);
    provision.set_workflow(workflow.clone());
    let project = std::env::var("ASGARD_PROJECT").ok();
    let server = asgard_mcp::AsgardMcp::new(
        core.catalog,
        gateway,
        registry,
        workflow,
        provision,
        project,
    );
    asgard_mcp::serve_stdio(server)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

fn build_allowlist(config: &Config) -> GroupAllowlist {
    GroupAllowlist::new(config.groups.clone())
}

fn build_registration_policy(config: &Config) -> RegistrationPolicy {
    RegistrationPolicy {
        require_manager: config.registration.require_manager,
        require_group: config.registration.require_group,
    }
}

fn build_requirements(config: &Config) -> ClassificationRequirements {
    ClassificationRequirements::from_overrides(config.classification_requirements.clone())
}

/// Resolve operator `review_depth` overrides over the shipped per-tier defaults:
/// any tier present replaces that tier's depth; absent tiers keep the default.
fn build_review_depth(config: &Config) -> ReviewDepthMap {
    let mut map = ReviewDepthMap::default();
    for (tier, cfg) in &config.review_depth {
        map = map.with_override(
            tier,
            ReviewDepth {
                skip: cfg.skip,
                standard_ids: cfg.standards.clone(),
                max_rounds: cfg.max_rounds,
            },
        );
    }
    map
}

/// Dev/test escape hatch: run the review gate against the mock model when no real
/// LLM is wired. Off by default — production review needs real LLM access.
fn review_allow_mock() -> bool {
    matches!(
        std::env::var("ASGARD_REVIEW_ALLOW_MOCK").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn build_review_config(config: &Config) -> ReviewConfig {
    ReviewConfig {
        poc_window_days: config.review.poc_window_days,
        auto_extensions: config.review.auto_extensions,
    }
}

fn build_governance_config(config: &Config) -> GovernanceConfig {
    GovernanceConfig {
        maintainer_min: config.governance.maintainer_min,
    }
}

/// Build the provisioning service: the embedded manifest catalog (+ operator
/// overlay), the dry-run `stub` connector and an always-on `exec` connector, the
/// gateway cost source, the builtin secret store, plus the `terraform` connector
/// (the universal provisioning path, incl. all AWS resources), the AWS cost
/// sources, and the guardrails the operator configures in asgard.yaml.
fn build_provision(db: Db, config: &Config, leases: Option<(Leases, i64)>) -> ProvisionService {
    let mut svc = ProvisionService::new(ProvisionRepo::new(db.clone()));
    // The exec connector is harmless without a manifest command, so it's always
    // available; gateway spend is always a cost source.
    svc.register_backend("exec", Arc::new(ExecConnector::new()));
    svc.register_cost_source(
        "gateway",
        Arc::new(GatewaySource::new(GatewayRepo::new(db.clone()))),
    );

    // Databricks billing (system.billing.usage) — env-driven like the inference
    // module, so it's available with or without a `provisioning:` block. Manifests
    // opt in via `cost.source.type: databricks-billing`; the daily rollup loop and
    // the manual `POST /api/cost/rollup` button drive it (no real-time polling).
    if let (Ok(host), Ok(token), Ok(warehouse)) = (
        std::env::var("DATABRICKS_HOST"),
        std::env::var("DATABRICKS_TOKEN"),
        std::env::var("DATABRICKS_WAREHOUSE_ID"),
    ) {
        svc.register_cost_source(
            "databricks-billing",
            Arc::new(DatabricksCostSource::new(host, token, warehouse)),
        );
        tracing::info!("databricks-billing cost source registered (system.billing.usage)");
    }

    // Per-project LiteLLM keys: env-driven like the Databricks block, before the
    // `provisioning:` early return so it's available without one. The connector
    // mints a budgeted virtual key per project (`litellm-key` service); the cost
    // source pulls each key's spend back. Creds absent → the manifest's `litellm`
    // connector falls back to `stub` and the cost source is simply not registered,
    // so e2e (no proxy) stays green.
    if let (Ok(base), Ok(master)) = (
        std::env::var("LITELLM_BASE_URL"),
        std::env::var("LITELLM_MASTER_KEY"),
    ) {
        svc.register_backend(
            "litellm",
            Arc::new(LiteLlmConnector::new(base.clone(), master.clone())),
        );
        svc.register_cost_source(
            "litellm",
            Arc::new(LiteLlmCostSource::new(
                base,
                master,
                ProvisionRepo::new(db.clone()),
            )),
        );
        tracing::info!("litellm connector + cost source registered (per-project keys)");
    }

    // The builtin secret-store master key is honored unconditionally — env var
    // (preferred) or the `provisioning.secrets` config block — *before* the
    // provisioning early-return below. A container-first deploy that sets only
    // ASGARD_SECRET_KEY and authors no config file must still get its key;
    // otherwise the store silently keeps the insecure dev key while everything
    // looks fine.
    let master_key = secret_master_key(config.provisioning.as_ref());
    if let Some(key) = master_key {
        svc.set_secret_store(
            Arc::new(BuiltinSecretStore::new(db.clone(), key)) as Arc<dyn SecretStore>
        );
    }

    // Per-resource run-log: every connector run (apply/destroy, success or
    // failure) is captured here, encrypted with the same master key as TF state
    // and read-gated to ViewAudit. Built unconditionally so the stub fallback
    // captures in dev/e2e too; the stub is re-registered to carry the store
    // (ProvisionService::new makes it before the store exists).
    let run_log = Arc::new(RunLogStore::new(
        db.clone(),
        master_key.unwrap_or(DEV_SECRET_KEY),
    ));
    svc.register_backend(
        "stub",
        Arc::new(StubProvisioner::new().with_run_log(run_log.clone())),
    );
    svc.set_run_log(run_log.clone());

    // Provisioning arms from an `asgard.yaml` block or, for a container-first
    // deploy with no config file, from env alone (ASGARD_TF_MODULES_DIR + friends).
    let env_provisioning = provisioning_from_env();
    let Some(p) = config.provisioning.as_ref().or(env_provisioning.as_ref()) else {
        return svc;
    };

    // Manifest overlay.
    if let Some(dir) = &p.services_dir {
        match ServiceCatalog::load(Some(dir)) {
            Ok(cat) => svc.set_catalog(cat),
            Err(e) => tracing::warn!("service overlay {} failed: {e}", dir.display()),
        }
    }

    // Terraform connector (the universal path). State is snapshotted into the DB
    // (encrypted with the master key) around every run, so the work_dir is just
    // scratch and may be ephemeral — durability rides on the database.
    if let Some(tf) = &p.terraform {
        let work = tf
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("asgard-tf"));
        let tf_state = Arc::new(TfStateStore::new(
            db.clone(),
            master_key.unwrap_or(DEV_SECRET_KEY),
        ));
        let mut connector = TerraformConnector::new(tf.bin.clone(), tf.modules_dir.clone(), work)
            .with_state(tf_state)
            .with_run_log(run_log.clone());
        if let Some((leases, ttl)) = &leases {
            connector = connector.with_leases(leases.clone(), *ttl);
        }
        svc.register_backend("terraform", Arc::new(connector));
        tracing::info!(
            "terraform connector registered (modules={}, durable state in DB)",
            tf.modules_dir.display()
        );
        if !tf.modules_dir.exists() {
            tracing::warn!(
                "terraform modules_dir {} does not exist — provisioning requests will fail until it is present (the container bundles modules at /modules)",
                tf.modules_dir.display()
            );
        }
    }

    // Exec cost source (open billing escape hatch).
    if !p.exec_cost_command.is_empty() {
        svc.register_cost_source(
            "exec",
            Arc::new(ExecCostSource::new(p.exec_cost_command.clone())),
        );
    }

    if let (Some(c), Some(a)) = (&p.default_cloud, &p.default_account) {
        svc.set_default_target(c.clone(), a.clone());
    }
    if !p.allowed.is_empty() {
        svc.set_allowed(
            p.allowed
                .iter()
                .map(|t| CloudTarget {
                    cloud: t.cloud.clone(),
                    account: t.account.clone(),
                })
                .collect(),
        );
    }
    svc.set_rollup_config(
        p.forecast_window_days.unwrap_or(0),
        p.anomaly_z.unwrap_or(0.0),
    );
    // Self-service ceilings: defaults, overridden by an asgard.yaml block, then by
    // ASGARD_AUTO_APPROVE_CEILINGS (per-tier merge) so an image-only deploy can
    // tune them without a config file or a recompile.
    let mut auto = AutoApprovePolicy::default();
    if let Some(aa) = p.auto_approve.as_ref().filter(|aa| !aa.ceilings.is_empty()) {
        auto.ceilings = aa.ceilings.clone();
    }
    if let Some(env_ceilings) = auto_approve_ceilings_from_env() {
        auto.ceilings.extend(env_ceilings);
    }
    svc.set_auto_approve(auto);
    // AWS cost sources (provisioning is handled by the terraform connector). The
    // Cost Explorer reads are independent of any provisioning arming.
    if let Some(aws) = &p.aws {
        let ce_live = aws.cost_explorer.unwrap_or(aws.execute);
        svc.register_cost_source(
            "aws-cost-explorer",
            Arc::new(AwsCostExplorerSource::new(aws.profile.clone(), ce_live)),
        );
        svc.register_cost_source(
            "account-total",
            Arc::new(AwsCostExplorerSource::account_total(
                aws.profile.clone(),
                ce_live,
            )),
        );
        tracing::info!(
            "aws cost-explorer + account-total registered (cost_explorer={}, region={})",
            ce_live,
            aws.region
        );
    }
    svc
}

/// Synthesize a provisioning config from env so a container-first deploy can arm
/// provisioning without an `asgard.yaml`. Returns `None` unless
/// `ASGARD_TF_MODULES_DIR` is set (the minimum to register the terraform
/// connector). `ASGARD_TF_WORK_DIR` sets the scratch dir (state lives in the DB);
/// `ASGARD_TF_ALLOWED` is a comma-separated `cloud:account` allowlist (a request
/// to anything not listed is refused). A config-file `provisioning:` block, when
/// present, takes precedence over this entirely.
/// `ASGARD_AUTO_APPROVE_CEILINGS` is a comma-separated `classification=usd` list,
/// e.g. `poc=500,light-operational=2500`. Merged onto the defaults per tier.
fn auto_approve_ceilings_from_env() -> Option<std::collections::BTreeMap<String, f64>> {
    let raw = std::env::var("ASGARD_AUTO_APPROVE_CEILINGS").ok()?;
    let map: std::collections::BTreeMap<String, f64> = raw
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(k, v)| {
            v.trim()
                .parse::<f64>()
                .ok()
                .map(|n| (k.trim().to_string(), n))
        })
        .collect();
    (!map.is_empty()).then_some(map)
}

fn provisioning_from_env() -> Option<ProvisioningCfg> {
    let modules_dir = std::env::var("ASGARD_TF_MODULES_DIR").ok();
    let services_dir = std::env::var("ASGARD_SERVICES_DIR").ok();
    // Arm if either the terraform modules or a service-definition overlay is set,
    // so a customer can add their own `service.yaml`s (over the embedded catalog)
    // by pointing one env var at a dir — whether that dir is baked into a derived
    // image, an EFS/volume mount, or synced from object storage by a sidecar.
    if modules_dir.is_none() && services_dir.is_none() {
        return None;
    }
    Some(provisioning_cfg_from_parts(
        modules_dir,
        services_dir,
        std::env::var("ASGARD_TF_WORK_DIR").ok(),
        std::env::var("ASGARD_TF_ALLOWED").ok(),
        std::env::var("ASGARD_AWS_DEFAULT_ACCOUNT").ok(),
    ))
}

fn provisioning_cfg_from_parts(
    modules_dir: Option<String>,
    services_dir: Option<String>,
    work_dir: Option<String>,
    allowed_csv: Option<String>,
    aws_default_account: Option<String>,
) -> ProvisioningCfg {
    let mut allowed: Vec<TargetCfg> = allowed_csv
        .map(|s| {
            s.split(',')
                .filter_map(|t| t.split_once(':'))
                .map(|(cloud, account)| TargetCfg {
                    cloud: cloud.trim().to_string(),
                    account: account.trim().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // `ASGARD_AWS_DEFAULT_ACCOUNT` sets the AWS-wide default target. Since a deploy
    // provisions into that account, make sure it's an allowed target (prepend it,
    // so it's also the default below) — otherwise the request gate would refuse it.
    if let Some(acct) = aws_default_account
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
    {
        if !allowed
            .iter()
            .any(|t| t.cloud == "aws" && t.account == acct)
        {
            allowed.insert(
                0,
                TargetCfg {
                    cloud: "aws".to_string(),
                    account: acct,
                },
            );
        }
    }
    // Default the request target to the first allowed entry, so a single-cloud
    // env-armed deploy provisions without every request having to name the
    // cloud/account — otherwise requests fall back to the stub target and are
    // refused by the (now env-replaced) allowlist.
    let (default_cloud, default_account) = match allowed.first() {
        Some(t) => (Some(t.cloud.clone()), Some(t.account.clone())),
        None => (None, None),
    };
    ProvisioningCfg {
        default_cloud,
        default_account,
        allowed,
        services_dir: services_dir.map(PathBuf::from),
        // The terraform connector arms only when a modules dir is given; a
        // services-only overlay still loads (its services use other connectors).
        terraform: modules_dir.map(|m| TerraformCfg {
            bin: default_tf_bin(),
            modules_dir: PathBuf::from(m),
            work_dir: work_dir.map(PathBuf::from),
        }),
        ..Default::default()
    }
}

/// The builtin store master key: `ASGARD_SECRET_KEY` env (64 hex chars) overrides
/// config; `None` leaves the dev default in place.
fn secret_master_key(p: Option<&ProvisioningCfg>) -> Option<[u8; 32]> {
    let hex = std::env::var("ASGARD_SECRET_KEY").ok().or_else(|| {
        p.and_then(|p| p.secrets.as_ref())
            .and_then(|s| s.master_key_hex.clone())
    })?;
    let bytes = (0..hex.len())
        .step_by(2)
        .filter_map(|i| {
            hex.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
        })
        .collect::<Vec<u8>>();
    if bytes.len() == 32 {
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        Some(k)
    } else {
        tracing::warn!("ASGARD secret key must be 64 hex chars (32 bytes); ignoring");
        None
    }
}

async fn reconcile_all(
    catalog: &CatalogRepo,
    schemas: &SchemaRegistry,
    sources: &[SourceCfg],
    git_token: Option<String>,
) {
    for s in sources {
        if let Some(provider) = make_provider(s, git_token.clone()) {
            match asgard_catalog::reconcile(catalog, schemas, provider.as_ref()).await {
                Ok(r) => tracing::info!(
                    "reconciled {}: +{} ~{} -{} ({} invalid)",
                    provider.source_id(),
                    r.inserted,
                    r.updated,
                    r.removed,
                    r.invalid.len()
                ),
                Err(e) => tracing::warn!("reconcile of {} failed: {e}", provider.source_id()),
            }
        } else {
            tracing::warn!("skipping malformed source: provider={}", s.provider);
        }
    }
}

fn make_provider(s: &SourceCfg, token: Option<String>) -> Option<Box<dyn SourceProvider>> {
    let git_ref = s.git_ref.clone().unwrap_or_else(|| "main".to_string());
    match s.provider.as_str() {
        "github" => Some(Box::new(GitHubProvider::new(
            s.owner.clone()?,
            s.repo.clone()?,
            git_ref,
            token,
        ))),
        "gitlab" => Some(Box::new(GitLabProvider::new(
            s.project.clone()?,
            git_ref,
            token,
        ))),
        "fixture" => Some(Box::new(FixtureProvider::new(s.path.clone()?))),
        _ => None,
    }
}

/// Load the service catalog (embedded standard modules + optional operator
/// overlay) — the source of truth for inference modules and provisionable services.
fn load_service_catalog(config: &Config) -> ServiceCatalog {
    let dir = config
        .provisioning
        .as_ref()
        .and_then(|p| p.services_dir.clone());
    ServiceCatalog::load(dir.as_deref()).unwrap_or_else(|e| {
        tracing::warn!("service catalog load failed: {e}; using embedded defaults");
        ServiceCatalog::embedded().expect("embedded service catalog")
    })
}

/// Build the gateway's providers + models from the enabled inference modules in
/// the catalog. A module is active when its credentials are present (master key,
/// or base URL for an OpenAI-compatible proxy like LiteLLM). The deterministic
/// mock is always present so the binary works offline and in tests. This is the
/// orchestrator frame: inference is a swappable service module, not bespoke core.
fn build_inference(
    catalog: &ServiceCatalog,
) -> (HashMap<String, Arc<dyn Provider>>, Vec<ModelInfo>) {
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("mock".to_string(), Arc::new(MockProvider));
    let mut models: Vec<ModelInfo> = Vec::new();

    for m in catalog.list() {
        let Some(inf) = &m.inference else { continue };
        // An empty env var is "not configured", not "configured blank" — a stray
        // `OPENAI_MASTER_KEY=` must not silently activate a provider.
        let key = std::env::var(&inf.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty());
        let base = inf
            .base_url
            .clone()
            .or_else(|| {
                inf.base_url_env
                    .as_ref()
                    .and_then(|e| std::env::var(e).ok())
            })
            .filter(|b| !b.trim().is_empty());
        let active = match inf.kind.as_str() {
            "openai" | "anthropic" => key.is_some(),
            // openai-compatible covers LiteLLM, vLLM, Databricks Model Serving, … —
            // all defined purely by manifest (base URL + optional request path).
            "openai-compatible" => base.is_some(),
            _ => false,
        };
        if !active {
            continue;
        }
        let provider: Arc<dyn Provider> = if inf.kind == "anthropic" {
            Arc::new(AnthropicProvider::new(key.clone().unwrap_or_default()))
        } else {
            let mut p = OpenAiProvider::new(key.clone().unwrap_or_default())
                .with_chat_path(inf.chat_path.clone());
            if let Some(b) = &base {
                p = p.with_base_url(b.clone());
            }
            Arc::new(p)
        };
        providers.insert(m.id.clone(), provider);
        for mm in &inf.models {
            let data_classes = if mm.data_classes.is_empty() {
                vec!["public".into(), "internal".into(), "confidential".into()]
            } else {
                mm.data_classes.clone()
            };
            models.push(ModelInfo {
                model_ref: mm.model_ref.clone(),
                provider: m.id.clone(),
                route_model: mm.route.clone(),
                data_classes,
                cost_in: mm.cost_in,
                cost_out: mm.cost_out,
            });
        }
        tracing::info!(
            "inference module '{}' active (kind={}, {} model(s))",
            m.id,
            inf.kind,
            inf.models.len()
        );
    }
    (providers, models)
}

fn default_mock_model() -> ModelInfo {
    ModelInfo {
        model_ref: "model:default/mock".to_string(),
        provider: "mock".to_string(),
        route_model: "mock".to_string(),
        data_classes: vec![
            "public".to_string(),
            "internal".to_string(),
            "confidential".to_string(),
        ],
        cost_in: 0.0005,
        cost_out: 0.0015,
    }
}

fn guardrail_mode() -> Mode {
    match std::env::var("ASGARD_GUARDRAIL_MODE").as_deref() {
        Ok("monitor") => Mode::Monitor,
        _ => Mode::Enforce,
    }
}

/// First-boot admin (rung 1). If no admin exists: use `ASGARD_ADMIN_PASSWORD`
/// when set, otherwise auto-generate one and log it once (the MinIO/Grafana
/// pattern) so a POC "just works" without ever shipping wide-open.
async fn maybe_seed_admin(identity: &IdentityService) {
    let user = std::env::var("ASGARD_ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    if identity
        .get_user_by_username(&user)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let (pw, generated) = match std::env::var("ASGARD_ADMIN_PASSWORD") {
        Ok(p) if !p.is_empty() => (p, false),
        _ => (
            format!("{}{}", asgard_storage::new_uid(), asgard_storage::new_uid()),
            true,
        ),
    };
    match identity
        .create_local_user(
            &user,
            &pw,
            None,
            Some("Administrator"),
            asgard_identity::Role::Admin,
        )
        .await
    {
        Ok(_) if generated => {
            tracing::warn!("──────────────────────────────────────────────────────────");
            tracing::warn!("no admin user existed and ASGARD_ADMIN_PASSWORD was unset.");
            tracing::warn!("generated an initial admin — shown once, change it after login:");
            tracing::warn!("    username: {user}");
            tracing::warn!("    password: {pw}");
            tracing::warn!("set ASGARD_ADMIN_PASSWORD to control this in future boots.");
            tracing::warn!("──────────────────────────────────────────────────────────");
        }
        Ok(_) => tracing::info!("seeded admin user '{user}'"),
        Err(e) => tracing::debug!("admin user not seeded: {e}"),
    }
}

/// Build the OIDC config (rung 2) from env. `ASGARD_OIDC_DOMAIN` derives the
/// Auth0-style endpoints; client id/secret/redirect are required. Returns `None`
/// (local-only) when the domain is unset.
fn build_oidc() -> Option<OidcConfig> {
    let domain = std::env::var("ASGARD_OIDC_DOMAIN")
        .ok()
        .filter(|s| !s.is_empty())?;
    let domain = domain.trim_end_matches('/');
    // Domain is set, so the operator intends SSO — fail loud (not silently off)
    // if the rest is incomplete, instead of the SSO button mysteriously missing.
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let (client_id, client_secret, redirect_uri) = match (
        var("ASGARD_OIDC_CLIENT_ID"),
        var("ASGARD_OIDC_CLIENT_SECRET"),
        var("ASGARD_OIDC_REDIRECT_URI"),
    ) {
        (Some(id), Some(secret), Some(uri)) => (id, secret, uri),
        (id, secret, uri) => {
            let mut missing = Vec::new();
            if id.is_none() {
                missing.push("ASGARD_OIDC_CLIENT_ID");
            }
            if secret.is_none() {
                missing.push("ASGARD_OIDC_CLIENT_SECRET");
            }
            if uri.is_none() {
                missing.push("ASGARD_OIDC_REDIRECT_URI");
            }
            tracing::warn!(
                "ASGARD_OIDC_DOMAIN is set but SSO is DISABLED — missing {}. Set all OIDC vars, or unset the domain to silence this.",
                missing.join(", ")
            );
            return None;
        }
    };
    let scopes = std::env::var("ASGARD_OIDC_SCOPES")
        .unwrap_or_else(|_| "openid email profile".to_string())
        .split_whitespace()
        .map(str::to_string)
        .collect();
    tracing::info!("OIDC login enabled (domain={domain})");
    Some(OidcConfig {
        authorize_endpoint: format!("https://{domain}/authorize"),
        token_endpoint: format!("https://{domain}/oauth/token"),
        userinfo_endpoint: format!("https://{domain}/userinfo"),
        client_id,
        client_secret,
        redirect_uri,
        scopes,
    })
}

/// Build OIDC role mapping from env. `None` when nothing is set — OIDC users
/// default to `member` and roles are managed manually (today's behavior).
/// `ASGARD_ADMIN_EMAILS` alone is a promote-only admin grant; setting
/// `ASGARD_OIDC_ADMIN_GROUPS` or `ASGARD_OIDC_FINANCE_GROUPS` turns on
/// authoritative group-claim sync (IdP owns the role, UI can't override).
fn build_oidc_roles() -> Option<OidcRoleConfig> {
    let list = |k: &str| -> Vec<String> {
        std::env::var(k)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    let admin_emails = list("ASGARD_ADMIN_EMAILS");
    let admin_groups = list("ASGARD_OIDC_ADMIN_GROUPS");
    let finance_groups = list("ASGARD_OIDC_FINANCE_GROUPS");
    if admin_emails.is_empty() && admin_groups.is_empty() && finance_groups.is_empty() {
        return None;
    }
    let groups_claim = std::env::var("ASGARD_OIDC_GROUPS_CLAIM")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "groups".to_string());
    let cfg = OidcRoleConfig {
        admin_groups,
        finance_groups,
        admin_emails,
        groups_claim,
    };
    if cfg.authoritative() {
        tracing::info!(
            "OIDC role sync enabled (claim={}): IdP groups are authoritative for admin/finance",
            cfg.groups_claim
        );
    } else {
        tracing::info!("OIDC admin-email allowlist enabled (promote-only)");
    }
    Some(cfg)
}

/// Resolve `ASGARD_DISABLE_LOCAL_LOGIN`. Refused (local login kept ON) unless
/// OIDC is configured, so the flag can never lock everyone out — the same
/// degrade-don't-trust posture as [`resolve_dev_insecure`].
fn resolve_disable_local_login(oidc_present: bool) -> bool {
    let on = matches!(
        std::env::var("ASGARD_DISABLE_LOCAL_LOGIN").as_deref(),
        Ok("1") | Ok("true")
    );
    if !on {
        return false;
    }
    if oidc_present {
        tracing::warn!(
            "ASGARD_DISABLE_LOCAL_LOGIN=1: local username/password sign-in DISABLED; SSO only."
        );
        true
    } else {
        tracing::error!(
            "ASGARD_DISABLE_LOCAL_LOGIN=1 ignored: OIDC is not configured, so disabling local login would lock everyone out. Local login stays ON."
        );
        false
    }
}

/// Resolve the dev escape hatch (rung 3). Only honored on a loopback bind; if set
/// against a non-loopback bind it is refused (enforcement stays on) with a loud
/// warning — it must never silently open a reachable deployment.
fn resolve_dev_insecure(bind: &str) -> bool {
    let on = matches!(
        std::env::var("ASGARD_DEV_INSECURE").as_deref(),
        Ok("1") | Ok("true")
    );
    if !on {
        return false;
    }
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if loopback {
        tracing::warn!(
            "ASGARD_DEV_INSECURE=1: human/admin auth enforcement DISABLED (loopback bind {bind}). For throwaway local use only."
        );
        true
    } else {
        tracing::warn!(
            "ASGARD_DEV_INSECURE=1 ignored: bind {bind} is not loopback. Auth enforcement stays ON."
        );
        false
    }
}

fn load_config(path: Option<PathBuf>) -> Config {
    let p = path.unwrap_or_else(|| PathBuf::from("asgard.yaml"));
    if p.exists() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(c) = serde_yaml::from_str::<Config>(&s) {
                return c;
            }
            tracing::warn!("failed to parse {}; ignoring", p.display());
        }
    }
    Config::default()
}

async fn static_handler(uri: Uri, system_name: String) -> Response {
    let raw = uri.path().trim_start_matches('/');
    // The embedded docs site owns everything under /docs. Branch here, before the
    // UI's SPA fallback, so a missing docs page serves the docs 404 — not the app.
    if raw == "docs" {
        return docs_handler("");
    }
    if let Some(rest) = raw.strip_prefix("docs/") {
        return docs_handler(rest);
    }
    let path = if raw.is_empty() { "index.html" } else { raw };
    // Serve the requested asset; fall back to the SPA shell for client-side routes.
    let (served, file) = match WebAssets::get(path) {
        Some(f) => (path, f),
        None => match WebAssets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    };
    let ct = mime_guess::from_path(served)
        .first_or_octet_stream()
        .as_ref()
        .to_string();
    if served == "index.html" {
        // Inject the configured system name into the shell <title> so the rebrand
        // shows on first paint (and with JS off), not just after the runtime
        // /api/auth/config fetch.
        let body = brand_index_html(&String::from_utf8_lossy(&file.data), &system_name);
        return ([(header::CONTENT_TYPE, ct)], body).into_response();
    }
    ([(header::CONTENT_TYPE, ct)], file.data.into_owned()).into_response()
}

/// Serve the embedded Docusaurus site. `rel` is the path under `/docs`. Docusaurus
/// emits pretty URLs as `<page>/index.html`, so a request with no file extension
/// falls back to the directory index.
fn docs_handler(rel: &str) -> Response {
    let rel = if rel.is_empty() {
        "index.html"
    } else {
        rel.trim_end_matches('/')
    };
    let mut tries = vec![rel.to_string()];
    if std::path::Path::new(rel).extension().is_none() {
        tries.push(format!("{rel}/index.html"));
    }
    for t in &tries {
        if let Some(f) = DocsAssets::get(t) {
            return serve_doc(t, f, StatusCode::OK);
        }
    }
    // Docs are bundled but this page isn't — serve the site's own 404. If the
    // 404 itself is missing, the docs weren't built into this binary.
    match DocsAssets::get("404.html") {
        Some(f) => serve_doc("404.html", f, StatusCode::NOT_FOUND),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            DOCS_NOT_BUNDLED,
        )
            .into_response(),
    }
}

fn serve_doc(name: &str, file: rust_embed::EmbeddedFile, status: StatusCode) -> Response {
    let ct = mime_guess::from_path(name)
        .first_or_octet_stream()
        .as_ref()
        .to_string();
    (status, [(header::CONTENT_TYPE, ct)], file.data.into_owned()).into_response()
}

/// Shown when this binary was built without the docs site (a bare `cargo build`
/// rather than the release image). The hosted copy is the fallback.
const DOCS_NOT_BUNDLED: &str = "<!doctype html><meta charset=utf-8><title>Docs not bundled</title>\
<body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.5\">\
<h1>Docs aren't bundled in this build</h1>\
<p>This Asgard binary was built without the documentation site. Build it with \
<code>cd docs &amp;&amp; npm ci &amp;&amp; npm run build</code> before compiling, or read the \
docs at <a href=\"https://asgard.dev\">asgard.dev</a>.</p>\
<p><a href=\"/\">← Back to Asgard</a></p>";

/// Replace the static `<title>Asgard</title>` shell title with the configured
/// system name. No-op when unset or still the default, so the stock build is
/// untouched. Keeps the UI a single embedded file (no build step) while making
/// the rebrand complete server-side.
fn brand_index_html(html: &str, system_name: &str) -> String {
    let name = system_name.trim();
    if name.is_empty() || name == "Asgard" {
        return html.to_string();
    }
    let safe = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    html.replace("<title>Asgard</title>", &format!("<title>{safe}</title>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_index_html_rewrites_title_only_when_configured() {
        let shell = "<html><head><title>Asgard</title></head></html>";
        // Default / unset → untouched.
        assert_eq!(brand_index_html(shell, "Asgard"), shell);
        assert_eq!(brand_index_html(shell, ""), shell);
        // Configured name → title rebranded.
        assert!(brand_index_html(shell, "Acme Corp").contains("<title>Acme Corp</title>"));
        // HTML-escaped so a stray angle bracket can't break out of the title.
        assert!(brand_index_html(shell, "A<b>").contains("<title>A&lt;b&gt;</title>"));
    }

    #[test]
    fn docs_handler_missing_page_is_404() {
        // A /docs request for a page that doesn't exist must 404 — never fall
        // through to the UI's SPA shell. Holds whether or not the docs site was
        // built into this binary.
        assert_eq!(
            docs_handler("definitely-not-a-real-page").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn env_arming_defaults_target_to_the_sole_allowed_entry() {
        let cfg = provisioning_cfg_from_parts(
            Some("/modules".into()),
            None,
            Some("/data/tf".into()),
            Some("auth0:psiq-tenant".into()),
            None,
        );
        // The default target matches the allowlist, so a request that doesn't name
        // a cloud/account still resolves to an allowed target (not the stub).
        assert_eq!(cfg.default_cloud.as_deref(), Some("auth0"));
        assert_eq!(cfg.default_account.as_deref(), Some("psiq-tenant"));
        assert_eq!(cfg.allowed.len(), 1);
        assert!(cfg.terraform.is_some());
    }

    #[test]
    fn env_arming_without_allowlist_sets_no_default_target() {
        let cfg = provisioning_cfg_from_parts(Some("/modules".into()), None, None, None, None);
        assert!(cfg.default_cloud.is_none());
        assert!(cfg.default_account.is_none());
        assert!(cfg.allowed.is_empty());
    }

    #[test]
    fn services_overlay_arms_without_terraform() {
        // A customer adds their own service definitions by pointing
        // ASGARD_SERVICES_DIR at an overlay dir — no terraform modules required.
        let cfg = provisioning_cfg_from_parts(None, Some("/srv/overlay".into()), None, None, None);
        assert_eq!(cfg.services_dir, Some(PathBuf::from("/srv/overlay")));
        assert!(
            cfg.terraform.is_none(),
            "no modules dir → terraform connector stays unarmed"
        );
    }

    #[test]
    fn aws_default_account_sets_target_and_arms_its_allowlist() {
        // ASGARD_AWS_DEFAULT_ACCOUNT alone makes (aws, <id>) the default target and
        // an allowed one, so a request provisions into it without naming it.
        let cfg = provisioning_cfg_from_parts(
            Some("/modules".into()),
            None,
            None,
            None,
            Some("123456789012".into()),
        );
        assert_eq!(cfg.default_cloud.as_deref(), Some("aws"));
        assert_eq!(cfg.default_account.as_deref(), Some("123456789012"));
        assert!(cfg
            .allowed
            .iter()
            .any(|t| t.cloud == "aws" && t.account == "123456789012"));
    }

    #[test]
    fn aws_default_account_prepends_to_an_existing_allowlist() {
        // It augments rather than replaces an explicit allowlist, and becomes the
        // default target (first entry).
        let cfg = provisioning_cfg_from_parts(
            Some("/modules".into()),
            None,
            None,
            Some("auth0:tenant".into()),
            Some("123456789012".into()),
        );
        assert_eq!(cfg.default_account.as_deref(), Some("123456789012"));
        assert_eq!(cfg.allowed.len(), 2);
    }
}
