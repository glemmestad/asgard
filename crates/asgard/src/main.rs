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
    LiteLlmCostSource, ProvisionRepo, ProvisionService, SecretStore, ServiceCatalog,
    TerraformConnector, TfStateStore, DEV_SECRET_KEY,
};
use asgard_registry::{
    ClassificationRequirements, GovernanceConfig, GroupAllowlist, GroupEntry, ProjectRegistry,
    RegistrationPolicy, ReviewConfig,
};
use asgard_storage::{leases::Leases, Db};
use asgard_workflow::WorkflowEngine;

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
    about = "Control plane for AI/agent development"
)]
struct Cli {
    #[arg(
        long,
        env = "ASGARD_DATABASE_URL",
        default_value = "sqlite://asgard.db",
        global = true
    )]
    database_url: String,
    #[arg(
        long,
        env = "ASGARD_URL",
        default_value = "http://localhost:8080",
        global = true
    )]
    url: String,
    #[arg(long, env = "ASGARD_TOKEN", global = true)]
    token: Option<String>,
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
    /// Catalog client commands.
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCmd,
    },
    /// Scaffold a golden-path agent template.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Gateway client commands.
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },
    /// Project registration + cost (the governed onboarding gate).
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// File an access request (request -> approval -> fulfillment).
    Request {
        #[arg(long)]
        model: String,
        #[arg(long, default_value = "internal")]
        data_class: String,
        #[arg(long, default_value = "user:default/cli")]
        as_user: String,
    },
    /// Validate a manifest file against its schema (offline).
    Validate { path: PathBuf },
}

#[derive(Subcommand)]
enum CatalogCmd {
    Ls {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        q: Option<String>,
    },
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
enum GatewayCmd {
    Login {
        #[arg(long)]
        project: String,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Register a project; mints a stable proj-YYYY-NNNN id.
    Register {
        #[arg(long)]
        name: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        manager: String,
        #[arg(long)]
        group: String,
        #[arg(long, default_value = "poc")]
        classification: String,
        #[arg(long, default_value = "internal")]
        data_class: String,
        #[arg(long, default_value_t = 0.0)]
        budget_usd: f64,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// List registered projects.
    Ls,
    /// Show spend rolled up by a dimension (project|owner|manager|group|...).
    Cost {
        #[arg(long, default_value = "project")]
        by: String,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Config {
    #[serde(default)]
    sources: Vec<SourceCfg>,
    #[serde(default)]
    reconcile_secs: Option<u64>,
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
}

impl Default for ReviewCfg {
    fn default() -> Self {
        ReviewCfg {
            poc_window_days: default_poc_window(),
            auto_extensions: default_auto_extensions(),
            sweep_secs: None,
        }
    }
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
    #[serde(default)]
    classifications: Vec<String>,
    #[serde(default)]
    max_resource_monthly_usd: f64,
    #[serde(default)]
    max_project_monthly_usd: f64,
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
    // Load a local .env (if present) before anything reads the environment, so
    // provider keys and service master credentials (OpenAI, AWS, Auth0, …) can
    // live in one gitignored file. Real env vars always win over .env entries.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Serve { bind, config } => serve(&cli.database_url, &bind, config).await?,
        Cmd::Mcp { config } => run_mcp(&cli.database_url, config).await?,
        Cmd::Init => {
            let p = asgard_cli::init_config(Path::new("."))?;
            println!("wrote {}", p.display());
        }
        Cmd::Catalog {
            cmd: CatalogCmd::Ls { kind, q },
        } => {
            let client = asgard_cli::Client::new(cli.url, cli.token);
            let entities = client.catalog_ls(kind.as_deref(), q.as_deref()).await?;
            for e in entities {
                println!(
                    "{}:{}/{}\t{}\t{}",
                    e.kind.to_lowercase(),
                    e.metadata.namespace,
                    e.metadata.name,
                    e.lifecycle,
                    e.metadata.title.unwrap_or_default()
                );
            }
        }
        Cmd::Agent {
            cmd: AgentCmd::New { template, name },
        } => {
            let dir = PathBuf::from(&name);
            let written = asgard_cli::agent_new(&template, &dir)?;
            println!("scaffolded {} files into {}/", written.len(), dir.display());
        }
        Cmd::Gateway {
            cmd: GatewayCmd::Login { project },
        } => {
            let client = asgard_cli::Client::new(cli.url, cli.token);
            let v = client.gateway_login(&project).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Project { cmd } => {
            let client = asgard_cli::Client::new(cli.url, cli.token);
            let v = match cmd {
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
                    client
                        .register_project(serde_json::json!({
                            "name": name,
                            "owner_email": owner,
                            "manager_email": manager,
                            "group": group,
                            "classification": classification,
                            "data_class": data_class,
                            "budget_usd": budget_usd,
                            "description": description,
                        }))
                        .await?
                }
                ProjectCmd::Ls => client.list_projects().await?,
                ProjectCmd::Cost { by } => client.cost(&by).await?,
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Request {
            model,
            data_class,
            as_user,
        } => {
            let subject = if model.contains(':') {
                model.clone()
            } else {
                format!("model:default/{model}")
            };
            let client = asgard_cli::Client::new(cli.url, cli.token);
            let v = client
                .submit_request(
                    "access",
                    &as_user,
                    &subject,
                    serde_json::json!({ "data_class": data_class }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Validate { path } => {
            let report = asgard_cli::validate_manifest(&path)?;
            println!("{report}");
        }
    }
    Ok(())
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
    let lease_ttl = config.lease_ttl_secs.unwrap_or(600) as i64;
    let leases = Leases::new(core.db.clone(), asgard_storage::new_uid());
    let provision = build_provision(core.db.clone(), &config, Some((leases.clone(), lease_ttl)));
    maybe_seed_admin(&core.identity).await;
    if let Err(e) = registry.seed_knowledge().await {
        tracing::warn!("seeding starter guidance/recipes failed: {e}");
    }

    // A platform-owned system project + gateway key so the dashboard's cost Q&A
    // works without a human pasting a key. Spend is attributed and governed like
    // any other project's calls.
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
    let provision = build_provision(core.db.clone(), &config, None);
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
            .with_state(tf_state);
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
    if let Some(aa) = &p.auto_approve {
        let mut auto = AutoApprovePolicy::default();
        if !aa.classifications.is_empty() {
            auto.classifications = aa.classifications.clone();
        }
        if aa.max_resource_monthly_usd > 0.0 {
            auto.max_resource_monthly_usd = aa.max_resource_monthly_usd;
        }
        if aa.max_project_monthly_usd > 0.0 {
            auto.max_project_monthly_usd = aa.max_project_monthly_usd;
        }
        svc.set_auto_approve(auto);
    }
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
fn provisioning_from_env() -> Option<ProvisioningCfg> {
    let modules_dir = std::env::var("ASGARD_TF_MODULES_DIR").ok()?;
    Some(provisioning_cfg_from_parts(
        modules_dir,
        std::env::var("ASGARD_TF_WORK_DIR").ok(),
        std::env::var("ASGARD_TF_ALLOWED").ok(),
    ))
}

fn provisioning_cfg_from_parts(
    modules_dir: String,
    work_dir: Option<String>,
    allowed_csv: Option<String>,
) -> ProvisioningCfg {
    let allowed: Vec<TargetCfg> = allowed_csv
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
        terraform: Some(TerraformCfg {
            bin: default_tf_bin(),
            modules_dir: PathBuf::from(modules_dir),
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
        let key = std::env::var(&inf.api_key_env).ok();
        let base = inf.base_url.clone().or_else(|| {
            inf.base_url_env
                .as_ref()
                .and_then(|e| std::env::var(e).ok())
        });
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
            "/modules".into(),
            Some("/data/tf".into()),
            Some("auth0:psiq-tenant".into()),
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
        let cfg = provisioning_cfg_from_parts("/modules".into(), None, None);
        assert!(cfg.default_cloud.is_none());
        assert!(cfg.default_account.is_none());
        assert!(cfg.allowed.is_empty());
    }
}
