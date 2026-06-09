//! Provisioning services through a manifest-driven catalog.
//!
//! A **service manifest** (one YAML per service) is the source of truth: it
//! declares how a service is provisioned (`provisioner.connector` + config) and
//! how its cost is attributed (`cost.source.type`). Adding a service is dropping
//! a manifest — no recompile. Three pluggable registries do the work: a
//! [`ServiceCatalog`] of manifests, a connector registry of [`Provisioner`]
//! backends (routing is by manifest connector, with `stub` as the dry-run
//! fallback), and a [`CostSourceRegistry`]. Credentials produced during
//! provisioning are written to a [`SecretStore`] and only a reference is recorded
//! — a secret value never enters a resource record, manifest, log, or audit
//! entry.
//!
//! Every resource belongs to a registered, active project (the gate) and is
//! tagged `project=<id>` so its cost rolls up alongside model spend. The flow:
//! `request` (gated) → policy decides auto-approve vs. review → on approval,
//! `do_provision` resolves the manifest's connector, runs `plan`/`apply`, routes
//! any secret outputs to the store, and records the resource.

pub mod connectors;
pub mod cost;
mod manifest;
mod repo;
mod runlog;
pub mod secrets;
mod stub;
mod tfstate;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;

use asgard_registry::{ProjectRegistry, Registration, RegistryError};
use asgard_storage::audit::{self, AuditRecord};
use asgard_storage::Db;
use asgard_workflow::{
    NewRequest, RequestFilter, State, WorkflowEngine, WorkflowError, WorkflowRequest,
};

pub use connectors::{ExecConnector, LiteLlmConnector, TerraformConnector};
pub use cost::{
    build_tree, movers, tagged_report, AnomalyRow, AwsCostExplorerSource, CostNode, CostRollupRepo,
    CostSource, CostSourceRegistry, DatabricksCostSource, DimRow, ExecCostSource, FlatSource,
    ForecastRow, GatewaySource, LiteLlmCostSource, Mover, Movers, ProjectFact, ProjectOverlay,
    RollupDim, RollupRow, TaggedReport,
};
pub use manifest::{
    class_rank, InferenceCfg, InferenceModel, Resolved, RetryCfg, ServiceCatalog, ServiceManifest,
    Variant, Variants,
};
pub use repo::{ProvisionRepo, ProvisionedRecord};
pub use runlog::{RunLogEntry, RunLogStore};
pub use secrets::{BuiltinSecretStore, SecretInfo, SecretRef, SecretStore};
pub use stub::StubProvisioner;
pub use tfstate::TfStateStore;

/// Dev master key for the builtin secret store when the operator configures
/// none. Production sets a real key (KMS/env/file) via `build_provision`; this
/// only keeps the single binary working out of the box.
pub const DEV_SECRET_KEY: [u8; 32] = [0x07; 32];

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("unsupported resource type: {0}")]
    Unsupported(String),
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    #[error("unresolved reference: {0}")]
    RefNotFound(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("state conflict: {0}")]
    Conflict(String),
    #[error("not permitted: {0}")]
    NotPermitted(String),
    #[error("request not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("workflow: {0}")]
    Workflow(#[from] WorkflowError),
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// One permitted provisioning target: a cloud backend + an account within it.
/// The router refuses any (cloud, account) not on this operator-configured list,
/// so a backend can never act outside its sanctioned accounts. This guardrail is
/// orthogonal to connector routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudTarget {
    pub cloud: String,
    pub account: String,
}

/// When a resource request may skip human approval: the self-service envelope is
/// a per-classification monthly ceiling. A request auto-approves when its
/// manifest opts in, the variant doesn't force review, and the project's total
/// committed infra spend (existing + this request) stays within the ceiling for
/// its classification. A classification with no configured ceiling never
/// auto-approves — every request at that tier routes to a human. Higher trust
/// tiers get larger envelopes; the project's own `budget_usd` is deliberately
/// not part of this — self-service authority is org policy, not self-set.
#[derive(Debug, Clone)]
pub struct AutoApprovePolicy {
    pub ceilings: BTreeMap<String, f64>,
}

impl Default for AutoApprovePolicy {
    fn default() -> Self {
        AutoApprovePolicy {
            ceilings: BTreeMap::from([
                ("poc".to_string(), 500.0),
                ("light-operational".to_string(), 2500.0),
                ("wide-operational".to_string(), 10_000.0),
                ("critical-path".to_string(), 25_000.0),
            ]),
        }
    }
}

impl AutoApprovePolicy {
    /// The self-service ceiling for a classification, or `None` if that tier
    /// never auto-approves.
    pub fn ceiling(&self, classification: &str) -> Option<f64> {
        self.ceilings.get(classification).copied()
    }
}

/// The cost-attribution + tagging context for one resource, derived centrally
/// from the project's registration so no backend can forget `project=<id>`.
#[derive(Debug, Clone)]
pub struct ResourceContext {
    pub project_id: String,
    pub owner: String,
    pub manager: String,
    pub group: String,
    pub cost_center: String,
    pub classification: String,
    pub environment: String,
    pub cloud: String,
    pub account: String,
}

impl ResourceContext {
    pub fn from_registration(reg: &Registration, cloud: &str, account: &str) -> Self {
        ResourceContext {
            project_id: reg.project_id.clone(),
            owner: reg.owner.clone(),
            manager: reg.manager.clone(),
            group: reg.group.clone(),
            cost_center: reg.cost_center.clone(),
            classification: reg.classification.clone(),
            environment: "dev".to_string(),
            cloud: cloud.to_string(),
            account: account.to_string(),
        }
    }

    /// The fixed tag set stamped on every provisioned resource. Built here, not
    /// by the backend, so cost attribution can't be skipped. This is also the
    /// `project=<id>` label the Terraform connector injects and every cost source
    /// filters on.
    pub fn tags(&self) -> BTreeMap<String, String> {
        let mut t = BTreeMap::new();
        t.insert("project".into(), self.project_id.clone());
        t.insert("owner".into(), self.owner.clone());
        t.insert("manager".into(), self.manager.clone());
        t.insert("group".into(), self.group.clone());
        t.insert("cost-center".into(), self.cost_center.clone());
        t.insert("classification".into(), self.classification.clone());
        t.insert("environment".into(), self.environment.clone());
        t.insert("cloud".into(), self.cloud.clone());
        t.insert("account".into(), self.account.clone());
        t.insert("managed-by".into(), "asgard".into());
        t
    }
}

#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub resource_type: String,
    pub name: String,
    pub ctx: ResourceContext,
    pub spec: serde_json::Value,
    /// The manifest's `provisioner.config`, handed to the connector verbatim.
    pub config: serde_json::Value,
    pub estimated_monthly_usd: f64,
    /// Output keys whose values are secrets: routed to the secret store.
    pub secret_outputs: Vec<String>,
    /// The provisioned-resource record id this run acts on, so a connector can
    /// attach its captured output to the resource's run-log. `None` ⇒ no capture
    /// (e.g. the dry-run `plan` gate, or a request built outside a driven record).
    pub resource_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub summary: String,
    pub tags: BTreeMap<String, String>,
    pub estimated_monthly_usd: f64,
}

#[derive(Debug, Clone)]
pub struct Provisioned {
    pub outputs: serde_json::Value,
    pub resource_ids: Vec<String>,
    /// Keys in `outputs` whose values are sensitive (e.g. Terraform's
    /// `sensitive = true` outputs). Unioned with the manifest's `secret_outputs`
    /// and routed to the secret store before the resource is recorded.
    pub sensitive_keys: Vec<String>,
}

/// The billing window a cost query covers, as inclusive-start/exclusive-end
/// `YYYY-MM-DD` dates (the granularity cloud billing APIs expect).
#[derive(Debug, Clone)]
pub struct CostWindow {
    pub start: String,
    pub end: String,
}

impl CostWindow {
    /// First of the current month through today (month-to-date). Cloud billing
    /// APIs reject an end date in the future, so the window ends "tomorrow" only
    /// up to today; here `end` is the day after today to make today inclusive.
    pub fn current_month() -> Self {
        use chrono::{Datelike, Duration, Utc};
        let today = Utc::now().date_naive();
        let start = today.with_day(1).unwrap_or(today);
        let end = today + Duration::days(1);
        CostWindow {
            start: start.format("%Y-%m-%d").to_string(),
            end: end.format("%Y-%m-%d").to_string(),
        }
    }
}

/// Actual incurred cost a source attributes to a project over a window.
/// `actual_usd` is `None` when the source exposes no usage feed — a free or
/// unmetered service — or the figure isn't available yet (cloud billing lags
/// real time by hours to a day). `source` records where the number came from so
/// a reader can tell a real $0 from "couldn't measure".
#[derive(Debug, Clone, Serialize)]
pub struct ServiceCost {
    pub backend: String,
    pub actual_usd: Option<f64>,
    pub source: String,
}

/// A provisioning connector (`stub`, `terraform`, `exec`, and — by
/// implementing this trait — `http`/`mcp`/custom). A connector defines how a
/// resource is created (`plan`/`apply`/`destroy`); cost attribution is a
/// separate [`CostSource`]. `plan` is always side-effect-free; `apply` must be
/// idempotent on `(account, resource_type, name)`; `destroy` tears a resource
/// down for project decommission / cleanup.
#[async_trait]
pub trait Provisioner: Send + Sync {
    fn name(&self) -> &str;
    fn dry_run(&self) -> bool;
    fn supports(&self, resource_type: &str) -> bool;
    async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError>;
    async fn apply(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
    ) -> Result<Provisioned, ProvisionError>;
    /// Tear down a previously-provisioned resource. `outputs` is what `apply`
    /// returned. Default is a no-op (nothing real was created).
    async fn destroy(
        &self,
        _req: &ProvisionRequest,
        _outputs: &serde_json::Value,
    ) -> Result<(), ProvisionError> {
        Ok(())
    }

    /// Suspend a resource to halt charges, reversibly (compute → stopped; storage
    /// has no meaningful stop). Returns `true` if it actually suspended — so the
    /// caller marks it `suspended` — and `false` for a service with no stop.
    /// Default: no-op `false`.
    async fn stop(
        &self,
        _req: &ProvisionRequest,
        _outputs: &serde_json::Value,
    ) -> Result<bool, ProvisionError> {
        Ok(false)
    }

    /// Resume a suspended resource. Returns `true` if it actually resumed (it's a
    /// stoppable service), `false` otherwise. Default: no-op `false`.
    async fn resume(
        &self,
        _req: &ProvisionRequest,
        _outputs: &serde_json::Value,
    ) -> Result<bool, ProvisionError> {
        Ok(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestOutcome {
    pub request: WorkflowRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisioned: Option<ProvisionedRecord>,
    /// True when the request is parked awaiting human approval (review-tier).
    pub pending_approval: bool,
}

/// One resource that a project-level lifecycle op (suspend/resume/destroy) could
/// not act on, with the connector error — surfaced so the action is best-effort
/// and the failure is visible rather than silent.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleFailure {
    pub resource: String,
    pub error: String,
}

/// The outcome of a project-level cascade over its resources. A given op fills
/// only the lists it produces (suspend → `suspended`/`skipped`, resume →
/// `resumed`, destroy → `destroyed`); `failed` collects best-effort errors.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LifecycleSummary {
    pub suspended: Vec<String>,
    pub resumed: Vec<String>,
    pub destroyed: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<LifecycleFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InfraResourceCost {
    pub rtype: String,
    pub name: String,
    pub backend: String,
    pub est_monthly_usd: f64,
    pub state: String,
}

/// A project's infrastructure cost: the deterministic manifest estimate (what
/// each live resource is expected to cost per month) alongside measured actuals
/// pulled from each resource's declared cost source. Model/token spend is one
/// such source (the gateway); a caller composes them for a full bill.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfraCost {
    pub project_id: String,
    pub estimated_monthly_usd: f64,
    pub actual: ServiceCost,
    pub resources: Vec<InfraResourceCost>,
}

/// Orchestrates the request → approve → fulfill → record loop on top of the
/// generic workflow engine, the manifest [`ServiceCatalog`], a registry of
/// pluggable connectors, the [`CostSourceRegistry`], a [`SecretStore`], an
/// operator account allowlist, and the auto-approval policy.
#[derive(Clone)]
pub struct ProvisionService {
    backends: HashMap<String, Arc<dyn Provisioner>>,
    catalog: ServiceCatalog,
    cost_sources: CostSourceRegistry,
    secrets: Arc<dyn SecretStore>,
    db: Db,
    default_cloud: String,
    default_account: String,
    allowed: Vec<CloudTarget>,
    auto: AutoApprovePolicy,
    repo: ProvisionRepo,
    forecast_window_days: i64,
    anomaly_z: f64,
    /// Handle the background worker uses to mark a request Fulfilled once its
    /// apply lands. `None` in bare unit tests (no async dispatch) — provisioning
    /// then runs inline and the workflow is left to the caller.
    workflow: Option<Arc<WorkflowEngine>>,
    /// This process's id, used as the row-lease `worker_owner` so a claim is
    /// attributable and a crashed claim is reclaimable.
    instance_id: String,
    /// How long a request waits inline for its apply before returning the
    /// `provisioning` record for the caller to poll (0 for `long_running`).
    wait_budget: Duration,
    /// Captures each connector run's output (encrypted, per resource) for the admin
    /// audit/debug view. `None` ⇒ capture is off (bare unit tests).
    run_log: Option<Arc<RunLogStore>>,
    /// Deployment-wide default cap on auto-retries of a failed apply/destroy; a
    /// service's `retry.max_attempts` overrides it, 0 disables auto-retry.
    max_retries: u32,
}

/// A work-state row is reclaimable once its claim heartbeat is older than this.
/// Comfortably above the connector's 600s lease TTL and the 60s heartbeat, so a
/// live apply is never reclaimed mid-flight; a crashed one recovers ~15 min on.
const STALE_SECS: i64 = 900;
/// Claim heartbeat cadence while a long apply/destroy runs.
const HEARTBEAT_SECS: u64 = 60;
/// Default auto-retry policy (overridable per service via `retry`, and fleet-wide
/// via `provision_max_retries`). 5 tries, 60s backoff doubling to a 1h cap.
const MAX_RETRIES: u32 = 5;
const RETRY_BASE_SECS: u64 = 60;
const RETRY_CAP_SECS: u64 = 3600;

/// What one `roll_up_costs` pass touched, for logging and the e2e proof.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RollupSummary {
    pub day: String,
    pub projects: usize,
    pub rows: usize,
    pub forecasts: usize,
    pub anomalies: usize,
}

impl ProvisionService {
    /// A service with the embedded manifest catalog, the dry-run `stub`
    /// connector, default cost sources, and the builtin secret store. Register
    /// real connectors with [`register_backend`](Self::register_backend).
    pub fn new(repo: ProvisionRepo) -> Self {
        let mut backends: HashMap<String, Arc<dyn Provisioner>> = HashMap::new();
        backends.insert("stub".to_string(), Arc::new(StubProvisioner::new()));
        let db = repo.db().clone();
        let secrets: Arc<dyn SecretStore> =
            Arc::new(BuiltinSecretStore::new(db.clone(), DEV_SECRET_KEY));
        ProvisionService {
            backends,
            catalog: ServiceCatalog::embedded().expect("embedded service catalog"),
            cost_sources: CostSourceRegistry::new(),
            secrets,
            db,
            default_cloud: "stub".to_string(),
            default_account: "local".to_string(),
            allowed: vec![CloudTarget {
                cloud: "stub".to_string(),
                account: "local".to_string(),
            }],
            auto: AutoApprovePolicy::default(),
            repo,
            forecast_window_days: 60,
            anomaly_z: 3.0,
            workflow: None,
            instance_id: asgard_storage::new_uid(),
            wait_budget: Duration::from_secs(5),
            run_log: None,
            max_retries: MAX_RETRIES,
        }
    }

    /// Give the service the workflow handle its background apply worker needs to
    /// transition a request to Fulfilled. Set in `build_provision` (and in tests
    /// that exercise the async path).
    pub fn set_workflow(&mut self, workflow: Arc<WorkflowEngine>) {
        self.workflow = Some(workflow);
    }

    /// Inline wait budget (seconds) a fast request blocks for completion before
    /// returning its `provisioning` record to poll. 0 disables the wait.
    pub fn set_wait_budget_secs(&mut self, secs: u64) {
        self.wait_budget = Duration::from_secs(secs);
    }

    /// Attach the run-log store so connector output is captured per resource.
    pub fn set_run_log(&mut self, store: Arc<RunLogStore>) {
        self.run_log = Some(store);
    }

    /// Deployment-wide auto-retry cap (0 disables auto-retry fleet-wide). A
    /// service's manifest `retry.max_attempts` still overrides this per service.
    pub fn set_max_retries(&mut self, n: u32) {
        self.max_retries = n;
    }

    /// The effective retry policy for a service: its manifest `retry` over the
    /// deployment defaults.
    fn retry_policy_for(&self, rtype: &str) -> RetryPolicy {
        let cfg = self.catalog.get(rtype).and_then(|m| m.retry.clone());
        RetryPolicy {
            max_attempts: cfg
                .as_ref()
                .and_then(|r| r.max_attempts)
                .unwrap_or(self.max_retries),
            base_secs: cfg
                .as_ref()
                .and_then(|r| r.base_secs)
                .unwrap_or(RETRY_BASE_SECS),
            cap_secs: cfg.and_then(|r| r.cap_secs).unwrap_or(RETRY_CAP_SECS),
        }
    }

    /// Register a connector under its name (`terraform`, `exec`, …).
    pub fn register_backend(
        &mut self,
        connector: impl Into<String>,
        backend: Arc<dyn Provisioner>,
    ) {
        self.backends.insert(connector.into(), backend);
    }

    pub fn register_cost_source(&mut self, key: impl Into<String>, src: Arc<dyn CostSource>) {
        self.cost_sources.register(key, src);
    }

    pub fn set_catalog(&mut self, catalog: ServiceCatalog) {
        self.catalog = catalog;
    }

    pub fn set_secret_store(&mut self, store: Arc<dyn SecretStore>) {
        self.secrets = store;
    }

    pub fn catalog(&self) -> &ServiceCatalog {
        &self.catalog
    }

    pub fn set_default_target(&mut self, cloud: impl Into<String>, account: impl Into<String>) {
        self.default_cloud = cloud.into();
        self.default_account = account.into();
    }

    pub fn set_allowed(&mut self, allowed: Vec<CloudTarget>) {
        self.allowed = allowed;
    }

    pub fn set_auto_approve(&mut self, auto: AutoApprovePolicy) {
        self.auto = auto;
    }

    /// The self-service monthly ceiling for a classification (`None` if that tier
    /// never auto-approves). Callers use it to apply the budget rule and to
    /// default a new project's budget to half the ceiling.
    pub fn auto_approve_ceiling(&self, classification: &str) -> Option<f64> {
        self.auto.ceiling(classification)
    }

    pub fn connectors(&self) -> Vec<String> {
        let mut v: Vec<String> = self.backends.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn repo(&self) -> &ProvisionRepo {
        &self.repo
    }

    /// Forecast trailing window (days) and the |z| anomaly threshold.
    pub fn set_rollup_config(&mut self, forecast_window_days: i64, anomaly_z: f64) {
        if forecast_window_days > 0 {
            self.forecast_window_days = forecast_window_days;
        }
        if anomaly_z > 0.0 {
            self.anomaly_z = anomaly_z;
        }
    }

    /// The persisted cost rollup store (the Phase-2 data layer the dashboard reads).
    pub fn rollup_repo(&self) -> CostRollupRepo {
        CostRollupRepo::new(self.db.clone())
    }

    /// A project's infrastructure cost: the manifest estimate per live resource
    /// plus measured actuals from each resource's declared cost source over
    /// `window`. Distinct sources are queried once and combined; a source with no
    /// live feed (or one not configured in this deployment) reports no actual and
    /// the estimate stands in.
    pub async fn project_cost(
        &self,
        project_id: &str,
        window: &CostWindow,
    ) -> Result<ProjectInfraCost, ProvisionError> {
        let records = self.repo.list_by_project(project_id).await?;
        let live: Vec<&ProvisionedRecord> = records
            .iter()
            .filter(|r| r.state == "provisioned")
            .collect();
        let resources: Vec<InfraResourceCost> = live
            .iter()
            .map(|r| InfraResourceCost {
                rtype: r.rtype.clone(),
                name: r.name.clone(),
                backend: r.backend.clone(),
                est_monthly_usd: r.est_monthly_usd,
                state: r.state.clone(),
            })
            .collect();
        let estimated_monthly_usd = resources.iter().map(|r| r.est_monthly_usd).sum();

        // Distinct cost sources across the project's live resources.
        let mut source_types: BTreeSet<String> = BTreeSet::new();
        for r in &live {
            if let Some(m) = self.catalog.get(&r.rtype) {
                source_types.insert(m.cost.source.source_type.clone());
            }
        }
        let mut total: Option<f64> = None;
        let mut labels: Vec<String> = Vec::new();
        let mut backend = String::new();
        for st in &source_types {
            match self.cost_sources.get(st) {
                Some(src) => {
                    let c = src.cost(project_id, window).await?;
                    if let Some(v) = c.actual_usd {
                        total = Some(total.unwrap_or(0.0) + v);
                    }
                    labels.push(c.source);
                    backend = c.backend;
                }
                None => labels.push(format!("{st} (unconfigured)")),
            }
        }
        let actual = ServiceCost {
            backend: if source_types.len() == 1 {
                backend
            } else {
                "multi".into()
            },
            actual_usd: total,
            source: if labels.is_empty() {
                "none".into()
            } else {
                labels.join(", ")
            },
        };
        Ok(ProjectInfraCost {
            project_id: project_id.to_string(),
            estimated_monthly_usd,
            actual,
            resources,
        })
    }

    /// Fan every project's cost sources into the persisted daily rollup for
    /// `day` (`YYYY-MM-DD`), then recompute each project's end-of-month forecast
    /// and flag anomalies. Idempotent per day — re-running overwrites. The day is
    /// a parameter (never `Utc::now()`) so a synthetic multi-day series is
    /// deterministic in tests; `serve()` passes today.
    pub async fn roll_up_costs(
        &self,
        registry: &ProjectRegistry,
        day: &str,
    ) -> Result<RollupSummary, ProvisionError> {
        let window = month_to_date_window(day);
        let month_start = month_start_str(day);
        let from_day = max_day(&month_start, &minus_days(day, self.forecast_window_days));
        let day_idx = day_of_month(day);
        let dim_count = days_in_month(day);
        let mut summary = RollupSummary {
            day: day.to_string(),
            ..Default::default()
        };

        let projects = registry.list().await?;
        let repo = self.rollup_repo();
        for reg in &projects {
            let pid = &reg.project_id;
            let records = self.repo.list_by_project(pid).await?;
            let mut est_by_source: BTreeMap<String, f64> = BTreeMap::new();
            for r in records.iter().filter(|r| r.state == "provisioned") {
                if let Some(m) = self.catalog.get(&r.rtype) {
                    *est_by_source
                        .entry(m.cost.source.source_type.clone())
                        .or_default() += m.cost.estimated_monthly_usd;
                }
            }
            // Model spend is always one of a project's sources when the gateway
            // source is wired in this deployment.
            if self.cost_sources.get("gateway").is_some() {
                est_by_source.entry("gateway".into()).or_insert(0.0);
            }
            if est_by_source.is_empty() {
                continue;
            }
            summary.projects += 1;

            let mut today: Vec<(String, f64)> = Vec::new();
            for (source_type, est_monthly) in &est_by_source {
                let Some(src) = self.cost_sources.get(source_type) else {
                    continue;
                };
                let measured = src.cost(pid, &window).await?;
                let cumulative = measured.actual_usd;
                let actual = match cumulative {
                    Some(c) => {
                        let prior = repo
                            .cumulative_for(pid, source_type, source_type, day)
                            .await?
                            .unwrap_or(0.0);
                        Some((c - prior).max(0.0))
                    }
                    None => None,
                };
                repo.upsert_daily(&RollupRow {
                    project_id: pid.clone(),
                    day: day.to_string(),
                    service: source_type.clone(),
                    source: source_type.clone(),
                    estimated_usd: est_monthly / dim_count,
                    actual_usd: actual,
                    cumulative_usd: cumulative,
                    owner: reg.owner.clone(),
                    manager: reg.manager.clone(),
                    cost_group: reg.group.clone(),
                    cost_center: reg.cost_center.clone(),
                    classification: reg.classification.clone(),
                })
                .await?;
                summary.rows += 1;
                if let Some(a) = actual {
                    today.push((source_type.clone(), a));
                }
            }

            let series = repo.series(pid, &from_day, day).await?;
            summary.anomalies += self
                .flag_anomalies(&repo, pid, day, &today, &series)
                .await?;
            if self
                .recompute_forecast(&repo, pid, day, day_idx, dim_count, &series)
                .await?
            {
                summary.forecasts += 1;
            }
        }
        Ok(summary)
    }

    /// Flag a day's per-source actuals that sit more than `anomaly_z` standard
    /// deviations from that source's trailing history. Needs ≥3 prior days.
    async fn flag_anomalies(
        &self,
        repo: &CostRollupRepo,
        project_id: &str,
        day: &str,
        today: &[(String, f64)],
        series: &[RollupRow],
    ) -> Result<usize, ProvisionError> {
        let mut n = 0;
        for (source, actual) in today {
            let hist: Vec<f64> = series
                .iter()
                .filter(|r| &r.source == source && r.day.as_str() < day)
                .filter_map(|r| r.actual_usd)
                .collect();
            if hist.len() < 3 {
                continue;
            }
            let mean = hist.iter().sum::<f64>() / hist.len() as f64;
            let var = hist.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / hist.len() as f64;
            let std = var.sqrt();
            if std < 1e-9 {
                continue;
            }
            let z = (actual - mean) / std;
            if z.abs() <= self.anomaly_z {
                continue;
            }
            let severity = if z.abs() >= 2.0 * self.anomaly_z {
                "high"
            } else {
                "medium"
            };
            repo.record_anomaly(&AnomalyRow {
                project_id: project_id.to_string(),
                day: day.to_string(),
                service: source.clone(),
                expected_usd: mean,
                actual_usd: *actual,
                z_score: (z * 100.0).round() / 100.0,
                severity: severity.to_string(),
            })
            .await?;
            n += 1;
        }
        Ok(n)
    }

    /// Refit the project's month-to-date cumulative actual and write an
    /// end-of-month projection. Honest about thin history: with <3 measured days
    /// no forecast is written (no fabricated number).
    async fn recompute_forecast(
        &self,
        repo: &CostRollupRepo,
        project_id: &str,
        day: &str,
        as_of_index: f64,
        days_in_month: f64,
        series: &[RollupRow],
    ) -> Result<bool, ProvisionError> {
        let month_start = month_start_str(day);
        let mut per_day: BTreeMap<String, f64> = BTreeMap::new();
        for r in series
            .iter()
            .filter(|r| r.day.as_str() >= month_start.as_str())
        {
            if let Some(a) = r.actual_usd {
                *per_day.entry(r.day.clone()).or_default() += a;
            }
        }
        let mut cum = 0.0;
        let mut points: Vec<(f64, f64)> = Vec::new();
        for (d, total) in &per_day {
            cum += total;
            points.push((day_of_month(d), cum));
        }
        let Some(fit) = cost::linreg(&points) else {
            return Ok(false);
        };
        let fc = cost::forecast_eom(&fit, as_of_index, days_in_month);
        repo.write_forecast(&ForecastRow {
            project_id: project_id.to_string(),
            as_of_day: day.to_string(),
            method: "linreg".into(),
            eom_usd: (fc.eom * 100.0).round() / 100.0,
            low_usd: (fc.low * 100.0).round() / 100.0,
            high_usd: (fc.high * 100.0).round() / 100.0,
            r2: Some((fit.r2 * 1000.0).round() / 1000.0),
            n_days: points.len() as i64,
        })
        .await?;
        Ok(true)
    }

    /// The org-cost tree for the month containing `as_of_day`: company → group →
    /// manager → owner → project, each node carrying MTD, EOM forecast (± band),
    /// budget, and budget pressure. Reads only the denormalized rollup rows plus
    /// each project's budget + latest forecast.
    pub async fn cost_tree(
        &self,
        registry: &ProjectRegistry,
        as_of_day: &str,
        scope: Option<&str>,
    ) -> Result<CostNode, ProvisionError> {
        let repo = self.rollup_repo().scoped(scope.map(|s| s.to_string()));
        let from = month_start_str(as_of_day);
        let facts = repo.project_facts(&from, as_of_day).await?;
        let mut overlay: BTreeMap<String, ProjectOverlay> = BTreeMap::new();
        for f in &facts {
            let budget = registry
                .get(&f.project_id)
                .await?
                .map(|r| r.budget_usd)
                .unwrap_or(0.0);
            let ov = match repo.latest_forecast(&f.project_id).await? {
                Some(fc) => ProjectOverlay {
                    budget_usd: budget,
                    eom_usd: fc.eom_usd,
                    forecast_band: (fc.high_usd - fc.eom_usd).max(0.0),
                    has_forecast: true,
                },
                None => ProjectOverlay {
                    budget_usd: budget,
                    ..Default::default()
                },
            };
            overlay.insert(f.project_id.clone(), ov);
        }
        Ok(build_tree(&facts, &overlay))
    }

    /// Top movers: this month's MTD vs the previous full month, by project and
    /// by group.
    pub async fn cost_movers(
        &self,
        as_of_day: &str,
        top: usize,
        scope: Option<&str>,
    ) -> Result<Movers, ProvisionError> {
        let repo = self.rollup_repo().scoped(scope.map(|s| s.to_string()));
        let cur_from = month_start_str(as_of_day);
        let prev_from = month_start_str(&minus_days(&cur_from, 1));
        let prev_to = minus_days(&cur_from, 1);
        let current = repo.project_facts(&cur_from, as_of_day).await?;
        let previous = repo.project_facts(&prev_from, &prev_to).await?;
        Ok(movers(&current, &previous, top))
    }

    /// Tagged spend over the month vs the cloud account total. Without an
    /// account-total cost source the denominator is unknown, so tagged-% reports
    /// `n/a` rather than a misleading 100% (Phase-2 seam).
    pub async fn cost_tagged(
        &self,
        as_of_day: &str,
        scope: Option<&str>,
    ) -> Result<TaggedReport, ProvisionError> {
        let repo = self.rollup_repo().scoped(scope.map(|s| s.to_string()));
        let from = month_start_str(as_of_day);
        let facts = repo.project_facts(&from, as_of_day).await?;
        let tagged: f64 = facts.iter().map(|f| f.mtd_usd).sum();
        let account_total = self.account_total(as_of_day).await?;
        Ok(tagged_report(tagged, account_total))
    }

    /// Answer a natural-language cost question, grounded in the rollup store and
    /// routed through the governed gateway (cost-attributed via `virtual_key`).
    #[allow(clippy::too_many_arguments)]
    pub async fn cost_qa(
        &self,
        gateway: &asgard_gateway::Gateway,
        virtual_key: &str,
        model: &str,
        data_class: Option<String>,
        as_of_day: &str,
        question: &str,
        budgets: HashMap<String, f64>,
    ) -> Result<String, ProvisionError> {
        cost::qa::answer_cost_question(
            gateway,
            self.rollup_repo(),
            virtual_key,
            model,
            data_class,
            as_of_day,
            question,
            budgets,
        )
        .await
        .map_err(|e| ProvisionError::Backend(format!("cost q&a: {e}")))
    }

    /// The whole cloud bill for the window, if an `account-total` cost source is
    /// wired (the un-tag-filtered denominator for tagged-%). `None` otherwise.
    async fn account_total(&self, as_of_day: &str) -> Result<Option<f64>, ProvisionError> {
        match self.cost_sources.get("account-total") {
            Some(src) => Ok(src
                .cost("*", &month_to_date_window(as_of_day))
                .await?
                .actual_usd),
            None => Ok(None),
        }
    }

    /// The connector backend for a name, falling back to the always-present
    /// `stub` (dry-run) when the named connector isn't registered in this
    /// deployment — so a manifest works out of the box and degrades safely.
    fn connector_backend(&self, connector: &str) -> Arc<dyn Provisioner> {
        self.backends
            .get(connector)
            .cloned()
            .unwrap_or_else(|| self.backends.get("stub").cloned().expect("stub registered"))
    }

    /// Submit a gated resource request. The target cloud/account ride in the
    /// spec (`cloud`, `account`) and default to the service defaults; the pair
    /// must be on the allowlist. A request **auto-approves** (and provisions
    /// inline) only when [`AutoApprovePolicy`] holds — POC classification, within
    /// the per-resource cost cap, and within the project's remaining headroom;
    /// otherwise it parks for human approval.
    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
        project_id: &str,
        resource_type: &str,
        name: &str,
        spec: serde_json::Value,
        requester: &str,
    ) -> Result<RequestOutcome, ProvisionError> {
        self.request_inner(
            workflow,
            registry,
            project_id,
            resource_type,
            name,
            spec,
            requester,
            None,
        )
        .await
    }

    /// Roll a live resource onto a new container image. Reads the resource's stored
    /// spec, swaps only `image`, and re-requests it through the normal governed path
    /// (validation + allowlist + auto-approval) — an in-place re-apply that preserves
    /// every other spec field (env/secrets/grants/cert). The connector keys state by
    /// name, so ECS registers a new task-def revision and rolls without recreating the
    /// service. Rejects a resource that isn't live or has no `image` field; a re-deploy
    /// of the same image is a no-op (the spec is unchanged).
    pub async fn deploy_image(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
        project_id: &str,
        resource_id: &str,
        image: &str,
        requester: &str,
    ) -> Result<RequestOutcome, ProvisionError> {
        let rec = self
            .repo
            .get(resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(resource_id.to_string()))?;
        if rec.project_id != project_id {
            return Err(ProvisionError::NotFound(resource_id.to_string()));
        }
        if rec.state != "provisioned" {
            return Err(ProvisionError::Conflict(format!(
                "resource {resource_id} is {} (must be provisioned to deploy an image)",
                rec.state
            )));
        }
        if rec.spec.get("image").is_none() {
            return Err(ProvisionError::InvalidSpec(format!(
                "resource {resource_id} has no `image` field to update"
            )));
        }
        let mut spec = rec.spec.clone();
        spec["image"] = serde_json::json!(image);
        self.request(
            workflow, registry, project_id, &rec.rtype, &rec.name, spec, requester,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_inner(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
        project_id: &str,
        resource_type: &str,
        name: &str,
        spec: serde_json::Value,
        requester: &str,
        config_override: Option<serde_json::Value>,
    ) -> Result<RequestOutcome, ProvisionError> {
        let reg = registry.require_active(project_id).await?;
        let manifest = self
            .catalog
            .get(resource_type)
            .ok_or_else(|| ProvisionError::Unsupported(resource_type.to_string()))?;
        self.catalog.validate_spec(resource_type, &spec)?;
        if name.trim().is_empty() {
            return Err(ProvisionError::InvalidSpec(
                "resource name is required".into(),
            ));
        }

        // Allowlist gate: unchanged — the target is the spec's cloud/account or the
        // global default, so which targets an operator has armed still governs.
        let account = spec
            .get("account")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_account)
            .to_string();
        let gate_cloud = spec
            .get("cloud")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_cloud)
            .to_string();
        let target = CloudTarget {
            cloud: gate_cloud.clone(),
            account: account.clone(),
        };
        if !self.allowed.contains(&target) {
            return Err(ProvisionError::NotPermitted(format!(
                "target {gate_cloud}/{account} is not an allowed provisioning target"
            )));
        }
        // Recorded cloud (attribution/tags only — not the gate): the resource's own
        // manifest cloud, so an AWS bucket isn't mislabeled with the default cloud
        // (e.g. an auth0-first allowlist tagging it `auth0`).
        let cfg = manifest.connector_config();
        let cloud = spec
            .get("cloud")
            .and_then(|v| v.as_str())
            .or_else(|| cfg.get("cloud").and_then(|v| v.as_str()))
            .unwrap_or(&self.default_cloud)
            .to_string();

        // Resolve the variant (cost/tier/approval keyed to the spec) and enforce
        // the service's tier availability — a hard gate, not a review.
        let resolved = manifest.resolve(&spec);
        if let Some(reason) = manifest.tier_violation(&reg.classification, &resolved) {
            return Err(ProvisionError::NotPermitted(reason));
        }

        // Auto-approval conditions: the manifest opts in, the variant doesn't force
        // review, the project is at an auto-approvable classification, and both the
        // per-resource and project-total cost limits hold (variant cost).
        let est = resolved.estimated_monthly_usd;
        // Count in-flight (`provisioning`) spend too, so two concurrent async
        // requests can't both clear the ceiling before either row flips to
        // `provisioned`.
        let infra_so_far = self.repo.infra_committed_for_project(project_id).await?;
        // An update (re-request of a live resource of the same name) keeps that
        // resource's estimate inside `infra_so_far`; adding `est` on top would
        // double-count it and could push a no-cost-change update (e.g. an image
        // bump) over the ceiling on its own already-counted spend. Subtract the
        // existing estimate so only the *delta* counts.
        let existing_est = self
            .repo
            .get_active_by_name(project_id, resource_type, name)
            .await?
            .map(|r| r.est_monthly_usd)
            .unwrap_or(0.0);
        let committed = reg.spent_usd + infra_so_far + est - existing_est;
        let within_ceiling = self
            .auto
            .ceiling(&reg.classification)
            .is_some_and(|ceiling| committed <= ceiling);
        let auto_ok = manifest.auto_approvable && !resolved.force_review && within_ceiling;
        let tier_str = if auto_ok { "self_service" } else { "review" };
        let sla_seconds = if auto_ok { None } else { Some(7 * 24 * 3600) };

        // The payload doubles as the policy context, so it must stay Cedar-safe
        // (strings/ints/bools only — no floats, no arbitrary nested records). The
        // raw spec is carried as a JSON string and the cost estimate is derived
        // from the manifest at provision time rather than passed through here.
        let mut payload = serde_json::json!({
            "project_id": project_id,
            "resource_type": resource_type,
            "name": name,
            "spec_json": spec.to_string(),
            "data_class": reg.data_class,
            "provision_tier": tier_str,
            "cloud": cloud,
            "account": account,
        });
        // A per-request connector-module override (the target's grant mechanism)
        // rides as a Cedar-safe string. `level` is surfaced to the policy context so
        // an operator can gate grants by access level without a code change.
        if let Some(m) = config_override
            .as_ref()
            .and_then(|c| c.get("module"))
            .and_then(|v| v.as_str())
        {
            payload["config_module"] = serde_json::json!(m);
        }
        if let Some(level) = spec.get("level").and_then(|v| v.as_str()) {
            payload["level"] = serde_json::json!(level);
        }
        let req = workflow
            .submit(NewRequest {
                kind: format!("provision:{resource_type}"),
                requester: requester.to_string(),
                subject: format!("{resource_type}/{name}"),
                payload,
                sla_seconds,
            })
            .await?;

        if req.state == State::Approved {
            let budget = self.budget_for(Some(manifest));
            self.enqueue_and_wait(workflow, &reg, req, budget).await
        } else {
            let pending = req.state == State::Requested;
            Ok(RequestOutcome {
                request: req,
                provisioned: None,
                pending_approval: pending,
            })
        }
    }

    /// Grant a consumer resource access to a target resource at `level`. Resolves
    /// the consumer's principal identity and the target's ARN + action set from
    /// their records and manifests, then flows the binding through the normal
    /// request/approve/fulfill lifecycle using the *target's* grant mechanism
    /// (`grant.module`). Same-project only — both resources must belong to
    /// `project_id` (the caller's authority over it is enforced upstream).
    #[allow(clippy::too_many_arguments)]
    pub async fn request_grant(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
        project_id: &str,
        consumer_resource_id: &str,
        target_resource_id: &str,
        level: &str,
        requester: &str,
    ) -> Result<RequestOutcome, ProvisionError> {
        let consumer = self
            .repo
            .get(consumer_resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(consumer_resource_id.to_string()))?;
        let target = self
            .repo
            .get(target_resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(target_resource_id.to_string()))?;

        if consumer.project_id != project_id || target.project_id != project_id {
            return Err(ProvisionError::NotPermitted(
                "consumer and target must belong to the same project".into(),
            ));
        }
        if consumer.state != "provisioned" || target.state != "provisioned" {
            return Err(ProvisionError::InvalidSpec(
                "both resources must be provisioned before granting access".into(),
            ));
        }

        let cm = self
            .catalog
            .get(&consumer.rtype)
            .ok_or_else(|| ProvisionError::Unsupported(consumer.rtype.clone()))?;
        let tm = self
            .catalog
            .get(&target.rtype)
            .ok_or_else(|| ProvisionError::Unsupported(target.rtype.clone()))?;

        let grant = tm.grant.as_ref().ok_or_else(|| {
            ProvisionError::InvalidSpec(format!("'{}' is not a grantable target", target.rtype))
        })?;
        let principal_kind = cm.principal_kind.as_deref().ok_or_else(|| {
            ProvisionError::InvalidSpec(format!(
                "'{}' cannot be granted access (declares no principal)",
                consumer.rtype
            ))
        })?;
        if principal_kind != grant.principal_kind {
            return Err(ProvisionError::InvalidSpec(format!(
                "principal-kind mismatch: '{}' provides '{}', '{}' grants to '{}'",
                consumer.rtype, principal_kind, target.rtype, grant.principal_kind
            )));
        }
        let principal_output = cm.principal_output.as_deref().ok_or_else(|| {
            ProvisionError::InvalidSpec(format!(
                "'{}' declares no principal_output",
                consumer.rtype
            ))
        })?;
        let principal_role_arn = consumer
            .outputs
            .get(principal_output)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ProvisionError::InvalidSpec(format!(
                    "consumer '{}' has no output '{principal_output}'",
                    consumer.name
                ))
            })?
            .to_string();
        let target_arn = target
            .outputs
            .get("arn")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ProvisionError::InvalidSpec(format!("target '{}' has no 'arn' output", target.name))
            })?
            .to_string();
        let actions = tm.access_levels.get(level).ok_or_else(|| {
            ProvisionError::InvalidSpec(format!(
                "'{}' has no access level '{level}' (known: {})",
                target.rtype,
                tm.access_levels
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        let spec = serde_json::json!({
            "consumer_resource_id": consumer_resource_id,
            "target_resource_id": target_resource_id,
            "level": level,
            "principal_role_arn": principal_role_arn,
            "target_arn": target_arn,
            "actions": actions,
        });
        let name = format!("grant-{}-{}-{}", consumer.name, target.name, level);
        let config_override = serde_json::json!({ "module": grant.module });
        self.request_inner(
            workflow,
            registry,
            project_id,
            "access-grant",
            &name,
            spec,
            requester,
            Some(config_override),
        )
        .await
    }

    /// Tear down a provisioned resource (project decommission / cleanup). Routes
    /// to the manifest's connector and marks the record destroyed.
    pub async fn deprovision(
        &self,
        resource_id: &str,
        actor: &str,
    ) -> Result<ProvisionedRecord, ProvisionError> {
        let _ = actor;
        let rec = self
            .repo
            .get(resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(resource_id.to_string()))?;
        if rec.state == "destroyed" || rec.state == "destroying" {
            return Ok(rec);
        }
        // Mark the work item, then drive the teardown in the background (or inline
        // without a workflow handle) and wait the service's budget for completion.
        self.repo.mark_state(resource_id, "destroying").await?;
        self.dispatch(resource_id).await?;
        let budget = self.budget_for(self.catalog.get(&rec.rtype));
        self.inline_wait(resource_id, budget).await
    }

    /// Manually retry a stuck resource now, bypassing the backoff window. Two cases:
    /// a `failed`/`destroy_failed` row is re-armed back to its work state; a row
    /// already in `provisioning`/`destroying` but stranded behind a dead worker is
    /// reclaimed *only if its claim is stale* (so a healthy in-flight apply is never
    /// yanked — that path is the operator's unstick button for an OOM/crash orphan
    /// that would otherwise wait on the slow reconcile sweep). A no-op for any other
    /// state, or for a work-state row a live worker is still driving.
    pub async fn retry_resource(
        &self,
        resource_id: &str,
    ) -> Result<ProvisionedRecord, ProvisionError> {
        let rec = self
            .repo
            .get(resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(resource_id.to_string()))?;
        match rec.state.as_str() {
            "failed" | "destroy_failed" => self.repo.reset_for_retry(resource_id).await?,
            "provisioning" | "destroying" => {
                if !self
                    .repo
                    .reclaim_stale(resource_id, &stale_cutoff())
                    .await?
                {
                    return Ok(rec);
                }
            }
            _ => return Ok(rec),
        }
        self.dispatch(resource_id).await?;
        let budget = self.budget_for(self.catalog.get(&rec.rtype));
        self.inline_wait(resource_id, budget).await
    }

    /// The captured connector runs (apply/destroy, success and failure) for a
    /// resource, oldest first (the attempt timeline). Empty when capture is off or
    /// the resource has not run yet. The caller is responsible for the `ViewAudit`
    /// gate — the output can contain provider secrets.
    pub async fn resource_runs(
        &self,
        resource_id: &str,
    ) -> Result<Vec<RunLogEntry>, ProvisionError> {
        match &self.run_log {
            Some(store) => store.list_for_resource(resource_id).await,
            None => Ok(vec![]),
        }
    }

    /// The connector + a re-drive request for an existing record — the shape every
    /// post-provision op (`destroy`/`stop`/`resume`) needs to call back into the
    /// connector with the resource's original spec and tags.
    fn backend_and_req(&self, rec: &ProvisionedRecord) -> (Arc<dyn Provisioner>, ProvisionRequest) {
        let (connector, config) = self.connector_and_config(&rec.rtype);
        let backend = self.connector_backend(&connector);
        let preq = ProvisionRequest {
            resource_type: rec.rtype.clone(),
            name: rec.name.clone(),
            ctx: ctx_from_record(rec),
            spec: rec.spec.clone(),
            config,
            estimated_monthly_usd: rec.est_monthly_usd,
            secret_outputs: vec![],
            resource_id: Some(rec.id.clone()),
        };
        (backend, preq)
    }

    /// Kill the charges, reversibly: suspend every `provisioned` resource via its
    /// connector's `stop`. Each transition commits before/after the connector call
    /// (`provisioned → suspending → suspended`) so the detail view can poll live;
    /// a service with no stop (storage) is `skipped` and stays `provisioned`. Only
    /// `provisioned` resources accrue estimated cost, so a `suspended` resource
    /// drops off the bill. Best-effort: a failing resource is restored to its
    /// prior state and recorded in `failed`.
    pub async fn suspend_project(
        &self,
        project_id: &str,
    ) -> Result<LifecycleSummary, ProvisionError> {
        let mut summary = LifecycleSummary::default();
        for rec in self.repo.list_by_project(project_id).await? {
            if rec.state != "provisioned" {
                continue;
            }
            self.repo.mark_state(&rec.id, "suspending").await?;
            let (backend, preq) = self.backend_and_req(&rec);
            match backend.stop(&preq, &rec.outputs).await {
                Ok(true) => {
                    self.repo.mark_state(&rec.id, "suspended").await?;
                    summary.suspended.push(rec.name);
                }
                Ok(false) => {
                    self.repo.mark_state(&rec.id, "provisioned").await?;
                    summary.skipped.push(rec.name);
                }
                Err(e) => {
                    self.repo.mark_state(&rec.id, "provisioned").await?;
                    summary.failed.push(LifecycleFailure {
                        resource: rec.name,
                        error: e.to_string(),
                    });
                }
            }
        }
        Ok(summary)
    }

    /// Reverse a suspend: resume every `suspended` resource via its connector's
    /// `resume`, transitioning `suspended → resuming → provisioned`. Best-effort;
    /// a failing resource stays `suspended` and is recorded in `failed`.
    pub async fn resume_project(
        &self,
        project_id: &str,
    ) -> Result<LifecycleSummary, ProvisionError> {
        let mut summary = LifecycleSummary::default();
        for rec in self.repo.list_by_project(project_id).await? {
            if rec.state != "suspended" {
                continue;
            }
            self.repo.mark_state(&rec.id, "resuming").await?;
            let (backend, preq) = self.backend_and_req(&rec);
            match backend.resume(&preq, &rec.outputs).await {
                Ok(_) => {
                    self.repo.mark_state(&rec.id, "provisioned").await?;
                    summary.resumed.push(rec.name);
                }
                Err(e) => {
                    self.repo.mark_state(&rec.id, "suspended").await?;
                    summary.failed.push(LifecycleFailure {
                        resource: rec.name,
                        error: e.to_string(),
                    });
                }
            }
        }
        Ok(summary)
    }

    /// Tear it all down, irreversibly: destroy every not-yet-`destroyed` resource
    /// via its connector's `destroy` (incl. data), transitioning `→ destroying →
    /// destroyed`. Best-effort: a failing resource is restored to its prior state
    /// and recorded in `failed` so decommission still proceeds.
    pub async fn destroy_project_resources(
        &self,
        project_id: &str,
    ) -> Result<LifecycleSummary, ProvisionError> {
        let mut summary = LifecycleSummary::default();
        for rec in self.repo.list_by_project(project_id).await? {
            if rec.state == "destroyed" {
                continue;
            }
            let prior = rec.state.clone();
            self.repo.mark_state(&rec.id, "destroying").await?;
            let (backend, preq) = self.backend_and_req(&rec);
            match backend.destroy(&preq, &rec.outputs).await {
                Ok(()) => {
                    self.repo.mark_state(&rec.id, "destroyed").await?;
                    summary.destroyed.push(rec.name);
                }
                Err(e) => {
                    self.repo.mark_state(&rec.id, &prior).await?;
                    summary.failed.push(LifecycleFailure {
                        resource: rec.name,
                        error: e.to_string(),
                    });
                }
            }
        }
        Ok(summary)
    }

    /// Fulfill an approved provisioning request (the approver path). Runs the
    /// connector, records the resource, then transitions the request to
    /// Fulfilled. On backend failure the request stays Approved (retryable).
    pub async fn fulfill(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
        request_id: &str,
        actor: &str,
    ) -> Result<RequestOutcome, ProvisionError> {
        let _ = actor;
        let req = workflow
            .get(request_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(request_id.to_string()))?;
        // Idempotent alias: a request already fulfilled (e.g. auto-enqueued on
        // approval) returns its record rather than erroring on a second fulfill.
        if req.state == State::Fulfilled {
            if let Some(rec) = self.repo.get_by_request(request_id).await? {
                return Ok(RequestOutcome {
                    request: req,
                    provisioned: Some(rec),
                    pending_approval: false,
                });
            }
        }
        if req.state != State::Approved {
            return Err(ProvisionError::NotPermitted(format!(
                "request is {}, must be approved before provisioning",
                req.state.as_str()
            )));
        }
        let project_id = req
            .payload
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProvisionError::InvalidSpec("request has no project_id".into()))?
            .to_string();
        let reg = registry.require_active(&project_id).await?;
        let resource_type = req
            .payload
            .get("resource_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let budget = self.budget_for(self.catalog.get(resource_type));
        self.enqueue_and_wait(workflow, &reg, req, budget).await
    }

    /// The connector name + its config for a service id (empty config if the
    /// manifest is gone).
    fn connector_and_config(&self, resource_type: &str) -> (String, serde_json::Value) {
        match self.catalog.get(resource_type) {
            Some(m) => (m.connector().to_string(), m.connector_config()),
            None => ("stub".to_string(), serde_json::json!({})),
        }
    }

    /// Persist the intent (a `provisioning` record) for an approved request, then
    /// hand it to the background worker and wait `budget` for completion. The
    /// record is written *before* any apply runs, so a dropped call / crash leaves
    /// a tracked row the reconciler heals — never an orphan.
    async fn enqueue_and_wait(
        &self,
        workflow: &WorkflowEngine,
        reg: &Registration,
        req: WorkflowRequest,
        budget: Duration,
    ) -> Result<RequestOutcome, ProvisionError> {
        let rec = self.enqueue_apply(reg, &req).await?;
        self.dispatch(&rec.id).await?;
        let rec = self.inline_wait(&rec.id, budget).await?;
        let request = workflow.get(&req.id).await?.unwrap_or(req);
        Ok(RequestOutcome {
            request,
            provisioned: Some(rec),
            pending_approval: false,
        })
    }

    /// Write the `provisioning` record (the durable work item) for an approved
    /// request, or reuse the existing one when this request was already enqueued
    /// (retry) or the name is already active. An identical re-request is fulfilled
    /// as satisfied by the existing work; a *changed* spec on a live resource
    /// re-arms that record for an in-place re-apply (an update). Tags/est are
    /// computed here exactly as the connector's `plan` would, so cost rollups are
    /// unchanged.
    async fn enqueue_apply(
        &self,
        reg: &Registration,
        req: &WorkflowRequest,
    ) -> Result<ProvisionedRecord, ProvisionError> {
        if let Some(existing) = self.repo.get_by_request(&req.id).await? {
            return Ok(existing);
        }
        let resource_type = req
            .payload
            .get("resource_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProvisionError::InvalidSpec("request has no resource_type".into()))?
            .to_string();
        let name = req
            .payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cloud = req
            .payload
            .get("cloud")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_cloud)
            .to_string();
        let account = req
            .payload
            .get("account")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_account)
            .to_string();
        let spec = req
            .payload
            .get("spec_json")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        if let Some(existing) = self
            .repo
            .get_active_by_name(&reg.project_id, &resource_type, &name)
            .await?
        {
            // A changed spec on a live resource is an update, not a duplicate:
            // re-arm the same record so this request re-applies it in place (the
            // connector keys state by name, so client_id/outputs survive). An
            // identical re-request — or one racing an in-flight apply — stays a
            // true no-op: fulfill it as satisfied by the existing work.
            if existing.state == "provisioned" && existing.spec != spec {
                let est = self
                    .catalog
                    .get(&resource_type)
                    .map(|m| m.resolve(&spec).estimated_monthly_usd)
                    .unwrap_or(existing.est_monthly_usd);
                self.repo
                    .update_for_reapply(&existing.id, &spec, est, &req.id)
                    .await?;
                return self
                    .repo
                    .get(&existing.id)
                    .await?
                    .ok_or_else(|| ProvisionError::NotFound(existing.id.clone()));
            }
            if let Some(wf) = &self.workflow {
                let _ = wf.fulfill(&req.id, "system").await;
            }
            return Ok(existing);
        }

        let manifest = self.catalog.get(&resource_type);
        let connector = manifest
            .map(|m| m.connector().to_string())
            .unwrap_or_else(|| "stub".to_string());
        let backend = self.connector_backend(&connector);
        let est = manifest
            .map(|m| m.resolve(&spec).estimated_monthly_usd)
            .unwrap_or(0.0);
        let ctx = ResourceContext::from_registration(reg, &cloud, &account);
        let now = asgard_storage::now();
        let rec = ProvisionedRecord {
            id: asgard_storage::new_uid(),
            project_id: reg.project_id.clone(),
            rtype: resource_type,
            name,
            spec,
            outputs: serde_json::json!({}),
            tags: ctx.tags(),
            est_monthly_usd: est,
            state: "provisioning".into(),
            backend: backend.name().to_string(),
            dry_run: backend.dry_run(),
            request_id: Some(req.id.clone()),
            created_at: now.clone(),
            updated_at: now,
            error: String::new(),
            attempts: 0,
            next_retry_at: None,
        };
        self.repo.record(&rec).await?;
        Ok(rec)
    }

    /// Run the work item: spawn the worker (production) or run it inline (no
    /// workflow handle wired — bare unit tests, which then provision synchronously).
    async fn dispatch(&self, id: &str) -> Result<(), ProvisionError> {
        if self.workflow.is_some() {
            let svc = self.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                if let Err(e) = svc.drive_core(&id).await {
                    tracing::warn!("provision worker {id} failed: {e}");
                }
            });
            Ok(())
        } else {
            self.drive_core(id).await
        }
    }

    /// The single idempotent worker body for one work-state row, called by the
    /// eager dispatch and by the reconciler. Claims the row (only one worker wins),
    /// heartbeats while the connector runs, then records the terminal state. Apply
    /// also fulfills the workflow request. A failed apply/destroy is recorded with
    /// its error and the next auto-retry armed (capped exponential backoff per the
    /// service's policy); once the cap is hit the row rests as `failed` until a
    /// manual `retry_resource`. The `failed`/`destroy_failed` states map back to
    /// Apply/Destroy so a re-drive picks them up.
    async fn drive_core(&self, id: &str) -> Result<(), ProvisionError> {
        let Some(rec) = self.repo.get(id).await? else {
            return Ok(());
        };
        let action = match rec.state.as_str() {
            "provisioning" | "failed" => Action::Apply,
            "destroying" | "destroy_failed" => Action::Destroy,
            _ => return Ok(()),
        };
        let stale = stale_cutoff();
        if !self
            .repo
            .claim(id, &rec.state, &self.instance_id, &stale)
            .await?
        {
            return Ok(());
        }
        let hb = self.spawn_heartbeat(id);
        let result = match action {
            Action::Apply => self.run_apply(&rec).await,
            Action::Destroy => self.run_destroy(&rec).await,
        };
        hb.abort();
        match result {
            Ok(outputs) => {
                // Fulfill the request before flipping the record to its terminal
                // state, so a caller that observes `provisioned` also sees the
                // workflow `Fulfilled` — no window for the inline wait to slip
                // between the two.
                if matches!(action, Action::Apply) {
                    if let (Some(wf), Some(req_id)) = (&self.workflow, &rec.request_id) {
                        let _ = wf.fulfill(req_id, "system").await;
                    }
                }
                let terminal = match action {
                    Action::Apply => "provisioned",
                    Action::Destroy => "destroyed",
                };
                self.repo.finish(id, terminal, &outputs).await?;
                Ok(())
            }
            Err(e) => {
                // Record the failure and arm the next auto-retry: bump attempts and
                // set the backoff deadline, or NULL once the per-service cap is hit
                // (the row then rests as failed until a manual retry).
                let terminal = match action {
                    Action::Apply => "failed",
                    Action::Destroy => "destroy_failed",
                };
                let policy = self.retry_policy_for(&rec.rtype);
                let attempts = rec.attempts + 1;
                let next = ((attempts as u32) < policy.max_attempts).then(|| {
                    asgard_storage::plus_seconds(
                        &asgard_storage::now(),
                        backoff_secs(attempts, &policy),
                    )
                });
                self.repo
                    .mark_failed(id, terminal, &e.to_string(), attempts, next.as_deref())
                    .await?;
                Err(e)
            }
        }
    }

    /// Resolve the connector + spec from a `provisioning` record and run
    /// `plan`+`apply`, routing secret outputs to the store. Returns the outputs to
    /// persist. Mirrors what the old inline `do_provision` did, minus the record
    /// write (the caller owns the terminal transition).
    async fn run_apply(
        &self,
        rec: &ProvisionedRecord,
    ) -> Result<serde_json::Value, ProvisionError> {
        let manifest = self.catalog.get(&rec.rtype);
        let connector = manifest
            .map(|m| m.connector().to_string())
            .unwrap_or_else(|| "stub".to_string());
        let mut config = manifest
            .map(|m| m.connector_config())
            .unwrap_or_else(|| serde_json::json!({}));
        // Per-request module override (a grant's target mechanism) rides on the
        // workflow payload, not the record — read it back for the apply.
        if let (Some(wf), Some(req_id)) = (&self.workflow, &rec.request_id) {
            if let Ok(Some(req)) = wf.get(req_id).await {
                if let Some(m) = req.payload.get("config_module").and_then(|v| v.as_str()) {
                    if let Some(obj) = config.as_object_mut() {
                        obj.insert("module".into(), serde_json::json!(m));
                    }
                }
            }
        }
        let secret_outputs = manifest
            .map(|m| m.secret_outputs.clone())
            .unwrap_or_default();
        let backend = self.connector_backend(&connector);
        if !backend.supports(&rec.rtype) {
            return Err(ProvisionError::Unsupported(format!(
                "connector '{connector}' does not support '{}'",
                rec.rtype
            )));
        }
        let preq = ProvisionRequest {
            resource_type: rec.rtype.clone(),
            name: rec.name.clone(),
            ctx: ctx_from_record(rec),
            spec: rec.spec.clone(),
            config,
            estimated_monthly_usd: rec.est_monthly_usd,
            secret_outputs,
            resource_id: Some(rec.id.clone()),
        };
        let plan = backend.plan(&preq).await?;
        let mut provisioned = backend.apply(&preq, &plan).await?;
        self.route_secrets(
            &rec.project_id,
            &rec.name,
            &preq.secret_outputs,
            &mut provisioned,
        )
        .await?;
        Ok(provisioned.outputs)
    }

    /// Tear down a `destroying` record via its connector; keeps the recorded
    /// outputs on the destroyed row.
    async fn run_destroy(
        &self,
        rec: &ProvisionedRecord,
    ) -> Result<serde_json::Value, ProvisionError> {
        let (backend, preq) = self.backend_and_req(rec);
        backend.destroy(&preq, &rec.outputs).await?;
        Ok(rec.outputs.clone())
    }

    /// Keep the claim heartbeat fresh while a long apply/destroy runs, so the
    /// reconciler's stale check doesn't reclaim in-flight work. Aborted on finish.
    fn spawn_heartbeat(&self, id: &str) -> tokio::task::JoinHandle<()> {
        let repo = self.repo.clone();
        let id = id.to_string();
        let owner = self.instance_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
                if repo.heartbeat(&id, &owner).await.is_err() {
                    break;
                }
            }
        })
    }

    /// Poll a record until it leaves its work state or `budget` elapses, then
    /// return the current record. Check-before-sleep so an instant (stub) apply
    /// returns without burning a tick; `budget == 0` returns the work-state record
    /// immediately (the `long_running` fast path).
    async fn inline_wait(
        &self,
        id: &str,
        budget: Duration,
    ) -> Result<ProvisionedRecord, ProvisionError> {
        let deadline = Instant::now() + budget;
        loop {
            let rec = self
                .repo
                .get(id)
                .await?
                .ok_or_else(|| ProvisionError::NotFound(id.to_string()))?;
            if !is_work_state(&rec.state) || Instant::now() >= deadline {
                return Ok(rec);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Inline-wait budget for a service: 0 for `long_running` (return immediately
    /// and poll), the configured budget otherwise.
    fn budget_for(&self, manifest: Option<&ServiceManifest>) -> Duration {
        if manifest.map(|m| m.long_running).unwrap_or(false) {
            Duration::ZERO
        } else {
            self.wait_budget
        }
    }

    /// Heal interrupted work. Sweep 1: re-drive stale/orphaned `provisioning` and
    /// `destroying` rows (a dropped call, a crashed worker, a redeploy mid-apply).
    /// Sweep 2: enqueue `Approved` provision requests that never got a record — the
    /// durable form of "approved ⇒ will deploy", covering an approve-then-crash gap.
    /// Lease-gated by the caller so only one replica runs it.
    pub async fn reconcile(
        &self,
        workflow: &WorkflowEngine,
        registry: &ProjectRegistry,
    ) -> Result<usize, ProvisionError> {
        let mut n = 0;
        let stale = stale_cutoff();
        for rec in self.repo.list_reclaimable(&stale, 50).await? {
            let _ = self.dispatch(&rec.id).await;
            n += 1;
        }
        // Sweep 1b: auto-retry failed rows whose backoff has elapsed (capped rows
        // have next_retry_at = NULL and are excluded). dispatch → drive_core re-drives.
        let now = asgard_storage::now();
        for rec in self.repo.list_retryable(&now, &stale, 50).await? {
            let _ = self.dispatch(&rec.id).await;
            n += 1;
        }
        let approved = workflow
            .list(&RequestFilter {
                state: Some(State::Approved),
                requester: None,
                subject: None,
                limit: Some(200),
            })
            .await?;
        for req in approved {
            if !req.kind.starts_with("provision:") {
                continue;
            }
            if self.repo.get_by_request(&req.id).await?.is_some() {
                continue;
            }
            let Some(project_id) = req.payload.get("project_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(reg) = registry.require_active(project_id).await else {
                continue;
            };
            let _ = self
                .enqueue_and_wait(workflow, &reg, req, Duration::ZERO)
                .await;
            n += 1;
        }
        Ok(n)
    }

    /// Move every sensitive output value into the secret store, replacing it in
    /// `outputs` with a `{secret_ref: …}` reference. The value never reaches the
    /// resource record, a log, or audit — only the ref does.
    async fn route_secrets(
        &self,
        project_id: &str,
        resource_name: &str,
        declared: &[String],
        provisioned: &mut Provisioned,
    ) -> Result<(), ProvisionError> {
        let keys: BTreeSet<String> = declared
            .iter()
            .cloned()
            .chain(provisioned.sensitive_keys.iter().cloned())
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        let serde_json::Value::Object(map) = &mut provisioned.outputs else {
            return Ok(());
        };
        for key in keys {
            let Some(value) = map.get(&key) else { continue };
            let value_str = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let secret_name = format!("{resource_name}-{key}");
            let sref = self
                .secrets
                .put(project_id, &secret_name, &value_str, None)
                .await?;
            map.insert(key, serde_json::json!({ SecretRef::KEY: sref }));
        }
        Ok(())
    }

    /// Fetch a secret value for a project (the caller's project is already
    /// verified at the MCP/API boundary). Audited; the value is never logged.
    pub async fn get_secret(
        &self,
        project_id: &str,
        name: &str,
        caller: &str,
    ) -> Result<String, ProvisionError> {
        let sref = SecretRef {
            store: self.secrets.name().to_string(),
            path: format!("{project_id}/{name}"),
            version: 0,
        };
        let value = self.secrets.get(&sref).await?;
        self.audit(caller, "secret.accessed", project_id, name)
            .await;
        Ok(value)
    }

    /// Rotate a secret to a fresh value (new version, stable ref). Audited.
    pub async fn rotate_secret(
        &self,
        project_id: &str,
        name: &str,
        caller: &str,
    ) -> Result<SecretRef, ProvisionError> {
        let sref = SecretRef {
            store: self.secrets.name().to_string(),
            path: format!("{project_id}/{name}"),
            version: 0,
        };
        let new_ref = self.secrets.rotate(&sref).await?;
        self.audit(caller, "secret.rotated", project_id, name).await;
        Ok(new_ref)
    }

    pub async fn list_secrets(&self, project_id: &str) -> Result<Vec<SecretInfo>, ProvisionError> {
        self.secrets.list(project_id).await
    }

    /// Rotate every secret past its `rotation_interval_days`. Returns the count
    /// rotated. Driven by the periodic reconcile loop.
    pub async fn rotate_due_secrets(&self) -> Result<usize, ProvisionError> {
        let now = asgard_storage::now();
        let due = self.secrets.due_for_rotation(&now).await?;
        let mut n = 0;
        for sref in due {
            if self.secrets.rotate(&sref).await.is_ok() {
                let (pid, name) = sref.path.split_once('/').unwrap_or((&sref.path, ""));
                self.audit("system:rotation", "secret.rotated", pid, name)
                    .await;
                n += 1;
            }
        }
        Ok(n)
    }

    async fn audit(&self, actor: &str, action: &str, project_id: &str, name: &str) {
        let rec = AuditRecord::new(actor, action)
            .entity(project_id)
            .outcome("allow")
            .data(serde_json::json!({ "secret": name }));
        if let Err(e) = audit::append(&self.db, &rec).await {
            tracing::warn!("audit append failed for {action}: {e}");
        }
    }
}

/// Today as `YYYY-MM-DD` (UTC). App-layer convenience for the read API and the
/// periodic rollup task — never called inside `roll_up_costs`, which takes the
/// day as a parameter for deterministic tests.
pub fn today() -> String {
    chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

fn parse_day(day: &str) -> chrono::NaiveDate {
    use chrono::NaiveDate;
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .unwrap_or_else(|_| NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
}

/// First of `day`'s month through the day after `day` (exclusive end), the
/// month-to-date window cloud billing sources expect.
fn month_to_date_window(day: &str) -> CostWindow {
    use chrono::{Datelike, Duration};
    let d = parse_day(day);
    let start = d.with_day(1).unwrap_or(d);
    let end = d + Duration::days(1);
    CostWindow {
        start: start.format("%Y-%m-%d").to_string(),
        end: end.format("%Y-%m-%d").to_string(),
    }
}

fn month_start_str(day: &str) -> String {
    use chrono::Datelike;
    let d = parse_day(day);
    d.with_day(1).unwrap_or(d).format("%Y-%m-%d").to_string()
}

fn minus_days(day: &str, n: i64) -> String {
    use chrono::Duration;
    (parse_day(day) - Duration::days(n))
        .format("%Y-%m-%d")
        .to_string()
}

fn max_day(a: &str, b: &str) -> String {
    if a >= b {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn days_in_month(day: &str) -> f64 {
    use chrono::Datelike;
    let d = parse_day(day);
    let (y, m) = (d.year(), d.month());
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first_next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    first_next
        .signed_duration_since(chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap())
        .num_days() as f64
}

fn day_of_month(day: &str) -> f64 {
    use chrono::Datelike;
    parse_day(day).day() as f64
}

/// Which connector op a work-state row needs driven.
#[derive(Clone, Copy)]
enum Action {
    Apply,
    Destroy,
}

/// States in which a row is an outstanding work item (the reconciler drives these
/// and the inline-wait blocks on them).
fn is_work_state(state: &str) -> bool {
    state == "provisioning" || state == "destroying"
}

/// The heartbeat cutoff before which a claimed row is considered abandoned.
fn stale_cutoff() -> String {
    asgard_storage::plus_seconds(&asgard_storage::now(), -STALE_SECS)
}

/// A service's resolved auto-retry policy (manifest over deployment defaults).
struct RetryPolicy {
    max_attempts: u32,
    base_secs: u64,
    cap_secs: u64,
}

/// Backoff before retry number `attempts` (1-based): exponential off `base_secs`,
/// capped at `cap_secs`. The shift is clamped so a large `max_attempts` can't
/// overflow before the `.min(cap)` clamps it anyway.
fn backoff_secs(attempts: i64, policy: &RetryPolicy) -> i64 {
    let shift = (attempts - 1).clamp(0, 16) as u32;
    policy
        .base_secs
        .saturating_mul(1u64 << shift)
        .min(policy.cap_secs) as i64
}

fn ctx_from_record(rec: &ProvisionedRecord) -> ResourceContext {
    let g = |k: &str| rec.tags.get(k).cloned().unwrap_or_default();
    ResourceContext {
        project_id: rec.project_id.clone(),
        owner: g("owner"),
        manager: g("manager"),
        group: g("group"),
        cost_center: g("cost-center"),
        classification: g("classification"),
        environment: g("environment"),
        cloud: g("cloud"),
        account: g("account"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asgard_catalog::CatalogRepo;
    use asgard_gateway::GatewayRepo;
    use asgard_policy::{CedarEngine, PolicyEngine};
    use asgard_registry::{GroupAllowlist, GroupEntry, RegisterInput, RegistrationPolicy};
    use asgard_storage::Db;

    async fn harness() -> (WorkflowEngine, ProjectRegistry, ProvisionService, String) {
        let path =
            std::env::temp_dir().join(format!("asgard-prov-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let policy: Arc<dyn PolicyEngine> = Arc::new(CedarEngine::new().unwrap());
        let workflow = WorkflowEngine::new(db.clone(), policy);
        let allow = GroupAllowlist::new(vec![GroupEntry {
            key: "platform".into(),
            display_name: "Platform".into(),
            cost_center: "CC-100".into(),
            active: true,
        }]);
        let registry = ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db.clone()),
            allow,
            RegistrationPolicy::default(),
        );
        let mut svc = ProvisionService::new(ProvisionRepo::new(db.clone()));
        svc.set_workflow(Arc::new(workflow.clone()));
        let reg = registry
            .register(
                RegisterInput {
                    name: "P".into(),
                    owner_email: "a@corp.example".into(),
                    manager_email: "b@corp.example".into(),
                    group: "platform".into(),
                    classification: None,
                    data_class: Some("internal".into()),
                    budget_usd: None,
                    description: None,
                    evidence: Default::default(),
                },
                "u",
            )
            .await
            .unwrap();
        (workflow, registry, svc, reg.project_id)
    }

    async fn register_with_class(registry: &ProjectRegistry, class: &str) -> String {
        registry
            .register(
                RegisterInput {
                    name: format!("proj-{class}"),
                    owner_email: format!("o-{class}@corp.example"),
                    manager_email: format!("m-{class}@corp.example"),
                    group: "platform".into(),
                    classification: Some(class.into()),
                    data_class: Some("internal".into()),
                    budget_usd: None,
                    description: None,
                    evidence: Default::default(),
                },
                "u",
            )
            .await
            .unwrap()
            .project_id
    }

    #[tokio::test]
    async fn self_service_resource_auto_provisions() {
        let (wf, reg, svc, pid) = harness().await;
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets"}),
                "user:default/a",
            )
            .await
            .unwrap();
        assert!(!out.pending_approval);
        assert_eq!(out.request.state, State::Fulfilled);
        let rec = out.provisioned.unwrap();
        assert_eq!(
            rec.tags.get("project").map(String::as_str),
            Some(pid.as_str())
        );
        assert!(rec.est_monthly_usd > 0.0);
        assert!(rec.dry_run);
        let found = svc.repo().get_by_request(&out.request.id).await.unwrap();
        assert_eq!(found.unwrap().rtype, "s3-bucket");
        assert_eq!(svc.repo().list_by_project(&pid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deprovision_tears_down_and_marks_destroyed() {
        let (wf, reg, svc, pid) = harness().await;
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        let rid = out.provisioned.unwrap().id;
        let destroyed = svc.deprovision(&rid, "user:default/a").await.unwrap();
        assert_eq!(destroyed.state, "destroyed");
        // The persisted record reflects the teardown.
        let rec = svc.repo().get(&rid).await.unwrap().unwrap();
        assert_eq!(rec.state, "destroyed");
    }

    #[tokio::test]
    async fn resource_cloud_comes_from_its_manifest_not_the_default() {
        // default_cloud is "stub" in the test service, but s3-bucket's manifest
        // declares cloud: aws — the record must carry the manifest's cloud (the
        // bug was an AWS bucket tagged with the global default).
        let (wf, reg, svc, pid) = harness().await;
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets"}),
                "user:default/a",
            )
            .await
            .unwrap();
        let rec = out.provisioned.unwrap();
        assert_eq!(rec.tags.get("cloud").map(String::as_str), Some("aws"));
    }

    /// Insert a provisioned record directly (bypassing a connector) so grant
    /// resolution can be exercised against realistic outputs.
    async fn seed_record(
        svc: &ProvisionService,
        pid: &str,
        rtype: &str,
        name: &str,
        outputs: serde_json::Value,
    ) -> String {
        let now = asgard_storage::now();
        let rec = ProvisionedRecord {
            id: asgard_storage::new_uid(),
            project_id: pid.to_string(),
            rtype: rtype.to_string(),
            name: name.to_string(),
            spec: serde_json::json!({}),
            outputs,
            tags: Default::default(),
            est_monthly_usd: 0.0,
            state: "provisioned".into(),
            backend: "stub".into(),
            dry_run: true,
            request_id: None,
            created_at: now.clone(),
            updated_at: now,
            error: String::new(),
            attempts: 0,
            next_retry_at: None,
        };
        let id = rec.id.clone();
        svc.repo().record(&rec).await.unwrap();
        id
    }

    const ROLE_ARN: &str = "arn:aws:iam::123456789012:role/proj-task-role";
    const BUCKET_ARN: &str = "arn:aws:s3:::proj-assets";

    #[tokio::test]
    async fn request_grant_self_service_write_auto_approves() {
        let (wf, reg, svc, pid) = harness().await;
        let consumer = seed_record(
            &svc,
            &pid,
            "ecs-service",
            "web",
            serde_json::json!({"task_role_arn": ROLE_ARN}),
        )
        .await;
        let target = seed_record(
            &svc,
            &pid,
            "s3-bucket",
            "assets",
            serde_json::json!({"arn": BUCKET_ARN}),
        )
        .await;

        let out = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "write",
                "user:default/a",
            )
            .await
            .unwrap();

        // Your own resources are self-service — no approval.
        assert!(!out.pending_approval);
        assert_eq!(out.request.state, State::Fulfilled);
        let rec = out.provisioned.unwrap();
        assert_eq!(rec.rtype, "access-grant");
        assert_eq!(rec.spec["principal_role_arn"], ROLE_ARN);
        assert_eq!(rec.spec["target_arn"], BUCKET_ARN);
        let actions: Vec<String> = serde_json::from_value(rec.spec["actions"].clone()).unwrap();
        assert!(actions.contains(&"s3:PutObject".to_string()));
        assert!(actions.contains(&"s3:GetObject".to_string()));
    }

    #[tokio::test]
    async fn request_grant_read_level_omits_write_actions() {
        let (wf, reg, svc, pid) = harness().await;
        let consumer = seed_record(
            &svc,
            &pid,
            "ecs-service",
            "web",
            serde_json::json!({"task_role_arn": ROLE_ARN}),
        )
        .await;
        let target = seed_record(
            &svc,
            &pid,
            "s3-bucket",
            "assets",
            serde_json::json!({"arn": BUCKET_ARN}),
        )
        .await;
        let out = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "read",
                "user:default/a",
            )
            .await
            .unwrap();
        let actions: Vec<String> =
            serde_json::from_value(out.provisioned.unwrap().spec["actions"].clone()).unwrap();
        assert!(actions.contains(&"s3:GetObject".to_string()));
        assert!(!actions.contains(&"s3:PutObject".to_string()));
    }

    #[tokio::test]
    async fn request_grant_unknown_level_is_rejected() {
        let (wf, reg, svc, pid) = harness().await;
        let consumer = seed_record(
            &svc,
            &pid,
            "ecs-service",
            "web",
            serde_json::json!({"task_role_arn": ROLE_ARN}),
        )
        .await;
        let target = seed_record(
            &svc,
            &pid,
            "s3-bucket",
            "assets",
            serde_json::json!({"arn": BUCKET_ARN}),
        )
        .await;
        let err = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "admin",
                "user:default/a",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("access level"), "got: {err}");
    }

    #[tokio::test]
    async fn request_grant_missing_principal_output_is_rejected() {
        let (wf, reg, svc, pid) = harness().await;
        let consumer = seed_record(&svc, &pid, "ecs-service", "web", serde_json::json!({})).await;
        let target = seed_record(
            &svc,
            &pid,
            "s3-bucket",
            "assets",
            serde_json::json!({"arn": BUCKET_ARN}),
        )
        .await;
        let err = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "write",
                "user:default/a",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("task_role_arn"), "got: {err}");
    }

    #[tokio::test]
    async fn request_grant_non_grantable_target_is_rejected() {
        let (wf, reg, svc, pid) = harness().await;
        let consumer = seed_record(
            &svc,
            &pid,
            "ecs-service",
            "web",
            serde_json::json!({"task_role_arn": ROLE_ARN}),
        )
        .await;
        // random-secret declares no `grant` mechanism.
        let target = seed_record(
            &svc,
            &pid,
            "random-secret",
            "api-key",
            serde_json::json!({"arn": "x"}),
        )
        .await;
        let err = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "write",
                "user:default/a",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("grantable"), "got: {err}");
    }

    #[tokio::test]
    async fn request_grant_cross_project_is_rejected() {
        let (wf, reg, svc, pid) = harness().await;
        let other = register_with_class(&reg, "light-operational").await;
        let consumer = seed_record(
            &svc,
            &pid,
            "ecs-service",
            "web",
            serde_json::json!({"task_role_arn": ROLE_ARN}),
        )
        .await;
        // Target lives in a different project.
        let target = seed_record(
            &svc,
            &other,
            "s3-bucket",
            "assets",
            serde_json::json!({"arn": BUCKET_ARN}),
        )
        .await;
        let err = svc
            .request_grant(
                &wf,
                &reg,
                &pid,
                &consumer,
                &target,
                "write",
                "user:default/a",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("same project"), "got: {err}");
    }

    #[tokio::test]
    async fn suspend_resume_destroy_cascade_transitions_state() {
        let (wf, reg, svc, pid) = harness().await;
        for name in ["assets", "cache"] {
            svc.request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                name,
                serde_json::json!({ "name": name }),
                "agent:default/a",
            )
            .await
            .unwrap();
        }
        let window = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };

        // Kill: the stub backend simulates a successful suspend.
        let s = svc.suspend_project(&pid).await.unwrap();
        assert_eq!(s.suspended.len(), 2);
        assert!(s.failed.is_empty());
        for r in svc.repo().list_by_project(&pid).await.unwrap() {
            assert_eq!(r.state, "suspended");
        }
        // Suspended resources fall off the estimated bill.
        assert_eq!(
            svc.project_cost(&pid, &window)
                .await
                .unwrap()
                .estimated_monthly_usd,
            0.0
        );

        // Un-kill resumes them.
        let r = svc.resume_project(&pid).await.unwrap();
        assert_eq!(r.resumed.len(), 2);
        for rec in svc.repo().list_by_project(&pid).await.unwrap() {
            assert_eq!(rec.state, "provisioned");
        }
        assert_eq!(
            svc.project_cost(&pid, &window)
                .await
                .unwrap()
                .estimated_monthly_usd,
            10.0
        );

        // Decommission tears everything down.
        let d = svc.destroy_project_resources(&pid).await.unwrap();
        assert_eq!(d.destroyed.len(), 2);
        for rec in svc.repo().list_by_project(&pid).await.unwrap() {
            assert_eq!(rec.state, "destroyed");
        }
    }

    #[tokio::test]
    async fn project_cost_sums_estimate_and_falls_back_to_no_actual() {
        let (wf, reg, svc, pid) = harness().await;
        for name in ["assets", "cache"] {
            svc.request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                name,
                serde_json::json!({ "name": name }),
                "user:default/a",
            )
            .await
            .unwrap();
        }
        let window = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };
        let cost = svc.project_cost(&pid, &window).await.unwrap();
        assert_eq!(cost.resources.len(), 2);
        assert_eq!(cost.estimated_monthly_usd, 10.0); // two s3 buckets @ 5.0
                                                      // aws-cost-explorer is not configured in tests, so no actual.
        assert!(cost.actual.actual_usd.is_none());
    }

    #[tokio::test]
    async fn project_cost_aggregates_across_sources() {
        let (wf, reg, mut svc, pid) = harness().await;
        // gateway source registered; exec-echo uses the flat source (no actual).
        svc.register_cost_source(
            "gateway",
            Arc::new(GatewaySource::new(GatewayRepo::new(svc.db.clone()))),
        );
        for (rtype, name, spec) in [
            ("exec-echo", "fn", serde_json::json!({"name": "fn"})),
            ("random-secret", "tok", serde_json::json!({"name": "tok"})),
        ] {
            svc.request(&wf, &reg, &pid, rtype, name, spec, "agent:default/a")
                .await
                .unwrap();
        }
        let window = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };
        let cost = svc.project_cost(&pid, &window).await.unwrap();
        // exec-echo @ 1.0 + random-secret @ 0.5
        assert_eq!(cost.estimated_monthly_usd, 1.5);
    }

    /// A cost source whose month-to-date figure the test scripts day by day, to
    /// drive `roll_up_costs` over a deterministic synthetic series.
    #[derive(Clone)]
    struct ScriptedSource {
        cumulative: Arc<std::sync::Mutex<Option<f64>>>,
    }

    #[async_trait]
    impl CostSource for ScriptedSource {
        fn name(&self) -> &str {
            "aws-cost-explorer"
        }
        async fn cost(&self, _p: &str, _w: &CostWindow) -> Result<ServiceCost, ProvisionError> {
            Ok(ServiceCost {
                backend: "aws".into(),
                actual_usd: *self.cumulative.lock().unwrap(),
                source: "scripted".into(),
            })
        }
    }

    async fn provision_s3(
        svc: &ProvisionService,
        wf: &WorkflowEngine,
        reg: &ProjectRegistry,
        pid: &str,
    ) {
        svc.request(
            wf,
            reg,
            pid,
            "s3-bucket",
            "assets",
            serde_json::json!({"name": "assets"}),
            "agent:default/a",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rollup_tracks_mtd_deltas_and_is_idempotent() {
        let (wf, reg, mut svc, pid) = harness().await;
        let cum = Arc::new(std::sync::Mutex::new(None));
        svc.register_cost_source(
            "aws-cost-explorer",
            Arc::new(ScriptedSource {
                cumulative: cum.clone(),
            }),
        );
        provision_s3(&svc, &wf, &reg, &pid).await;

        for d in 1..=5 {
            *cum.lock().unwrap() = Some(d as f64 * 10.0);
            svc.roll_up_costs(&reg, &format!("2026-06-{d:02}"))
                .await
                .unwrap();
        }
        // Re-running a day overwrites, never duplicates.
        *cum.lock().unwrap() = Some(50.0);
        svc.roll_up_costs(&reg, "2026-06-05").await.unwrap();

        let series = svc
            .rollup_repo()
            .series(&pid, "2026-06-01", "2026-06-30")
            .await
            .unwrap();
        assert_eq!(series.len(), 5, "one row per day, idempotent");
        for r in &series {
            assert_eq!(r.actual_usd, Some(10.0), "each day's delta is a flat 10");
        }
        assert_eq!(series.last().unwrap().cumulative_usd, Some(50.0));
    }

    #[tokio::test]
    async fn rollup_flags_a_spike_as_anomaly() {
        let (wf, reg, mut svc, pid) = harness().await;
        let cum = Arc::new(std::sync::Mutex::new(None));
        svc.register_cost_source(
            "aws-cost-explorer",
            Arc::new(ScriptedSource {
                cumulative: cum.clone(),
            }),
        );
        provision_s3(&svc, &wf, &reg, &pid).await;

        // Near-flat daily deltas (~5-6/day) then a large day-5 spike.
        for (d, mtd) in [(1, 5.0), (2, 11.0), (3, 16.0), (4, 22.0), (5, 102.0)] {
            *cum.lock().unwrap() = Some(mtd);
            svc.roll_up_costs(&reg, &format!("2026-06-{d:02}"))
                .await
                .unwrap();
        }
        let anomalies = svc.rollup_repo().anomalies(Some(&pid), 10).await.unwrap();
        assert_eq!(anomalies.len(), 1, "the day-5 jump should flag once");
        assert_eq!(anomalies[0].day, "2026-06-05");
        assert!(anomalies[0].z_score > 3.0);

        // A forecast was written once there were ≥3 measured days.
        assert!(svc
            .rollup_repo()
            .latest_forecast(&pid)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn review_resource_parks_for_approval_then_fulfills() {
        let (wf, reg, mut svc, pid) = harness().await;
        // rds-postgres is self-service by default now; drop the POC ceiling below its
        // estimate so it parks. The request→approve→fulfill cascade is what's under
        // test here, not the trigger that sends it to review.
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("poc".into(), 1.0)]),
        });
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "rds-postgres",
                "maindb",
                serde_json::json!({"name": "maindb"}),
                "user:default/a",
            )
            .await
            .unwrap();
        assert!(out.pending_approval, "review-tier must await approval");
        assert!(out.provisioned.is_none());
        assert_eq!(
            out.request.approver.as_deref(),
            Some("group:default/platform")
        );

        wf.approve(&out.request.id, "user:default/lead", Some("ok"))
            .await
            .unwrap();
        // rds-postgres is long_running: fulfill enqueues and returns the
        // `provisioning` record immediately while the apply runs in the
        // background. Poll to the terminal state; the workflow fulfills with it.
        let done = svc
            .fulfill(&wf, &reg, &out.request.id, "system")
            .await
            .unwrap();
        let rid = done.provisioned.unwrap().id;
        let rec = await_state(&svc, &rid, "provisioned").await;
        assert_eq!(rec.rtype, "rds-postgres");
        assert_eq!(
            wf.get(&out.request.id).await.unwrap().unwrap().state,
            State::Fulfilled
        );
    }

    #[tokio::test]
    async fn classification_without_ceiling_requires_approval() {
        let (wf, reg, mut svc, _pid) = harness().await;
        // Only POC has a self-service envelope; wide-operational has none, so even a
        // cheap, auto-approvable resource routes to a human at that tier.
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("poc".into(), 500.0)]),
        });
        let pid = register_with_class(&reg, "wide-operational").await;
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        assert!(
            out.pending_approval,
            "a classification with no configured ceiling must require approval"
        );
    }

    #[tokio::test]
    async fn over_cost_limit_requires_approval() {
        let (wf, reg, mut svc, pid) = harness().await;
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("poc".into(), 1.0)]), // s3 est 5.0 → over the ceiling
        });
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        assert!(
            out.pending_approval,
            "over the per-resource cost cap must require approval"
        );
    }

    #[tokio::test]
    async fn poc_under_limits_provisions_the_full_stack_unattended() {
        let (wf, reg, svc, pid) = harness().await;
        for (rtype, name, spec) in [
            ("ecr-repository", "app", serde_json::json!({"name": "app"})),
            ("s3-bucket", "assets", serde_json::json!({"name": "assets"})),
            (
                "dynamodb-table",
                "kv",
                serde_json::json!({"name": "kv", "pk_name": "id"}),
            ),
            (
                "ec2-instance",
                "worker",
                serde_json::json!({"name": "worker", "instance_type": "t3.micro"}),
            ),
            (
                "ecs-task",
                "job",
                serde_json::json!({"name": "job", "image": "stub/app:latest"}),
            ),
        ] {
            let out = svc
                .request(&wf, &reg, &pid, rtype, name, spec, "agent:default/builder")
                .await
                .unwrap();
            assert!(
                !out.pending_approval,
                "{rtype} should auto-provision for POC under limits"
            );
            assert_eq!(out.request.state, State::Fulfilled, "{rtype} not fulfilled");
        }
        assert_eq!(svc.repo().list_by_project(&pid).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn ec2_small_variant_auto_approves_big_variant_reviews() {
        let (wf, reg, mut svc, pid) = harness().await; // poc
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("poc".into(), 100.0)]),
        });
        let micro = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ec2-instance",
                "dev",
                serde_json::json!({"name": "dev", "instance_type": "t3.micro"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        assert!(
            !micro.pending_approval,
            "t3.micro ($7) is under the ceiling"
        );
        assert_eq!(micro.provisioned.unwrap().est_monthly_usd, 7.0);

        let big = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ec2-instance",
                "big",
                serde_json::json!({"name": "big", "instance_type": "m7i.4xlarge"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        assert!(
            big.pending_approval,
            "m7i.4xlarge ($500) is over the ceiling"
        );
    }

    #[tokio::test]
    async fn ec2_gpu_variant_is_tier_gated_for_poc() {
        let (wf, reg, svc, pid) = harness().await; // poc
        let e = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ec2-instance",
                "gpu",
                serde_json::json!({"name": "gpu", "instance_type": "g6.xlarge"}),
                "agent:default/a",
            )
            .await;
        assert!(
            matches!(e, Err(ProvisionError::NotPermitted(_))),
            "g6.xlarge requires wide-operational; a POC project is refused"
        );
    }

    #[tokio::test]
    async fn ec2_gpu_variant_forces_review_when_tier_and_cost_ok() {
        let (wf, reg, mut svc, _pid) = harness().await;
        let pid = register_with_class(&reg, "wide-operational").await;
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("wide-operational".into(), 100_000.0)]),
        });
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ec2-instance",
                "gpu",
                serde_json::json!({"name": "gpu", "instance_type": "g6.xlarge"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        assert!(
            out.pending_approval,
            "requires_approval forces human review even when tier + cost allow auto"
        );
    }

    #[tokio::test]
    async fn target_outside_allowlist_is_refused() {
        let (wf, reg, svc, pid) = harness().await;
        let e = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({"name": "assets", "cloud": "aws", "account": "999999999999"}),
                "agent:default/a",
            )
            .await;
        assert!(matches!(e, Err(ProvisionError::NotPermitted(_))));
    }

    #[tokio::test]
    async fn unregistered_project_cannot_request() {
        let (wf, reg, svc, _pid) = harness().await;
        let e = svc
            .request(
                &wf,
                &reg,
                "proj-2026-9999",
                "s3-bucket",
                "x",
                serde_json::json!({"name": "x"}),
                "u",
            )
            .await;
        assert!(matches!(e, Err(ProvisionError::Registry(_))));
    }

    #[tokio::test]
    async fn missing_required_field_rejected() {
        let (wf, reg, svc, pid) = harness().await;
        let e = svc
            .request(
                &wf,
                &reg,
                &pid,
                "dynamodb-table",
                "t",
                serde_json::json!({"name": "t"}),
                "u",
            )
            .await;
        assert!(matches!(e, Err(ProvisionError::InvalidSpec(_))));
    }

    #[tokio::test]
    async fn secret_outputs_are_stored_as_refs_not_values() {
        let (wf, reg, svc, pid) = harness().await;
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "random-secret",
                "api-key",
                serde_json::json!({"name": "api-key"}),
                "agent:default/a",
            )
            .await
            .unwrap();
        let rec = out.provisioned.unwrap();
        // The "value" output must be a ref, never a plaintext value.
        let value_out = &rec.outputs["value"];
        assert!(
            value_out.get(SecretRef::KEY).is_some(),
            "secret output must be a reference, got {value_out}"
        );
        // The stored secret is fetchable by the owning project and audited.
        let fetched = svc
            .get_secret(&pid, "api-key-value", "agent:default/a")
            .await
            .unwrap();
        assert!(!fetched.is_empty());
        // A different project cannot read it.
        let denied = svc
            .get_secret("proj-2026-0000", "api-key-value", "agent:default/intruder")
            .await;
        assert!(matches!(denied, Err(ProvisionError::RefNotFound(_))));
        // Rotation yields a new version; the ref path stays stable.
        let rotated = svc
            .rotate_secret(&pid, "api-key-value", "agent:default/a")
            .await
            .unwrap();
        assert!(rotated.version >= 2);
    }

    // ---- async provisioning engine ----

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    /// A connector whose `apply` parks on a semaphore, so a test can observe the
    /// in-flight `provisioning` window deterministically and count real applies.
    struct BlockingProvisioner {
        gate: Arc<Semaphore>,
        applies: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provisioner for BlockingProvisioner {
        fn name(&self) -> &str {
            "blocking"
        }
        fn dry_run(&self) -> bool {
            true
        }
        fn supports(&self, _r: &str) -> bool {
            true
        }
        async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError> {
            Ok(Plan {
                summary: String::new(),
                tags: req.ctx.tags(),
                estimated_monthly_usd: req.estimated_monthly_usd,
            })
        }
        async fn apply(
            &self,
            _req: &ProvisionRequest,
            _plan: &Plan,
        ) -> Result<Provisioned, ProvisionError> {
            self.gate.acquire().await.unwrap().forget();
            self.applies.fetch_add(1, Ordering::SeqCst);
            Ok(Provisioned {
                outputs: serde_json::json!({ "ok": true }),
                resource_ids: vec![],
                sensitive_keys: vec![],
            })
        }
    }

    fn provisioning_row(pid: &str, name: &str) -> ProvisionedRecord {
        let now = asgard_storage::now();
        ProvisionedRecord {
            id: asgard_storage::new_uid(),
            project_id: pid.to_string(),
            rtype: "s3-bucket".into(),
            name: name.into(),
            spec: serde_json::json!({ "name": name }),
            outputs: serde_json::json!({}),
            tags: BTreeMap::from([("project".to_string(), pid.to_string())]),
            est_monthly_usd: 10.0,
            state: "provisioning".into(),
            backend: "blocking".into(),
            dry_run: true,
            request_id: None,
            created_at: now.clone(),
            updated_at: now,
            error: String::new(),
            attempts: 0,
            next_retry_at: None,
        }
    }

    async fn await_state(svc: &ProvisionService, id: &str, want: &str) -> ProvisionedRecord {
        for _ in 0..300 {
            let r = svc.repo().get(id).await.unwrap().unwrap();
            if r.state == want {
                return r;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("resource {id} never reached state {want}");
    }

    #[tokio::test]
    async fn claim_is_exclusive_then_reclaimable_when_stale() {
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "c");
        svc.repo().record(&rec).await.unwrap();
        let now = asgard_storage::now();
        let past = asgard_storage::plus_seconds(&now, -600);
        let future = asgard_storage::plus_seconds(&now, 600);
        // Unclaimed → first owner wins; a live claim blocks a second owner.
        assert!(svc
            .repo()
            .claim(&rec.id, "provisioning", "owner-1", &past)
            .await
            .unwrap());
        assert!(!svc
            .repo()
            .claim(&rec.id, "provisioning", "owner-2", &past)
            .await
            .unwrap());
        // Once the heartbeat is stale (cutoff in the future), it's reclaimable.
        assert!(svc
            .repo()
            .claim(&rec.id, "provisioning", "owner-2", &future)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn infra_committed_counts_in_flight_but_total_does_not() {
        let (_wf, _reg, svc, pid) = harness().await;
        let mut a = provisioning_row(&pid, "a");
        a.est_monthly_usd = 50.0;
        svc.repo().record(&a).await.unwrap();
        let mut b = provisioning_row(&pid, "b");
        b.est_monthly_usd = 30.0;
        b.state = "provisioned".into();
        svc.repo().record(&b).await.unwrap();
        assert_eq!(
            svc.repo().infra_committed_for_project(&pid).await.unwrap(),
            80.0
        );
        assert_eq!(
            svc.repo().infra_total_for_project(&pid).await.unwrap(),
            30.0
        );
    }

    #[tokio::test]
    async fn list_reclaimable_finds_unclaimed_work() {
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "stale");
        svc.repo().record(&rec).await.unwrap();
        let future = asgard_storage::plus_seconds(&asgard_storage::now(), 600);
        let rows = svc.repo().list_reclaimable(&future, 50).await.unwrap();
        assert!(rows.iter().any(|r| r.id == rec.id));
    }

    #[tokio::test]
    async fn drive_core_provisions_an_orphaned_row() {
        // A `provisioning` record left behind by a crashed worker (no live driver)
        // converges to `provisioned` when the reconciler drives it. Routes to the
        // stub connector (s3-bucket's `terraform` is unregistered here).
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "orphan");
        svc.repo().record(&rec).await.unwrap();
        svc.drive_core(&rec.id).await.unwrap();
        let got = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(got.state, "provisioned");
    }

    #[tokio::test]
    async fn request_returns_provisioning_while_apply_is_in_flight() {
        let (wf, reg, mut svc, pid) = harness().await;
        let gate = Arc::new(Semaphore::new(0));
        let applies = Arc::new(AtomicUsize::new(0));
        svc.register_backend(
            "terraform",
            Arc::new(BlockingProvisioner {
                gate: gate.clone(),
                applies: applies.clone(),
            }),
        );
        svc.set_wait_budget_secs(0);
        let out = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "blk",
                serde_json::json!({ "name": "blk" }),
                "user:default/a",
            )
            .await
            .unwrap();
        // The record exists (intent persisted) but apply is parked, so it's still
        // provisioning and the request is not yet fulfilled.
        let rec = out.provisioned.unwrap();
        assert_eq!(rec.state, "provisioning");
        assert_eq!(out.request.state, State::Approved);
        assert_eq!(applies.load(Ordering::SeqCst), 0);
        assert_eq!(
            rec.tags.get("project").map(String::as_str),
            Some(pid.as_str())
        );
        // Release the apply; the background worker finishes and fulfills.
        gate.add_permits(1);
        let done = await_state(&svc, &rec.id, "provisioned").await;
        assert_eq!(done.state, "provisioned");
        assert_eq!(applies.load(Ordering::SeqCst), 1);
        let req = wf.get(&out.request.id).await.unwrap().unwrap();
        assert_eq!(req.state, State::Fulfilled);
    }

    #[tokio::test]
    async fn concurrent_drives_apply_exactly_once() {
        let (_wf, _reg, mut svc, pid) = harness().await;
        let applies = Arc::new(AtomicUsize::new(0));
        svc.register_backend(
            "terraform",
            Arc::new(BlockingProvisioner {
                gate: Arc::new(Semaphore::new(1)),
                applies: applies.clone(),
            }),
        );
        let rec = provisioning_row(&pid, "race");
        svc.repo().record(&rec).await.unwrap();
        // Two workers race the same row; the claim CAS lets exactly one apply.
        let (a, b) = tokio::join!(svc.drive_core(&rec.id), svc.drive_core(&rec.id));
        a.unwrap();
        b.unwrap();
        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert_eq!(
            svc.repo().get(&rec.id).await.unwrap().unwrap().state,
            "provisioned"
        );
    }

    #[tokio::test]
    async fn duplicate_request_is_idempotent() {
        let (wf, reg, svc, pid) = harness().await;
        let first = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "dup",
                serde_json::json!({ "name": "dup" }),
                "user:default/a",
            )
            .await
            .unwrap();
        let id1 = first.provisioned.unwrap().id;
        let second = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "dup",
                serde_json::json!({ "name": "dup" }),
                "user:default/a",
            )
            .await
            .unwrap();
        assert_eq!(second.provisioned.unwrap().id, id1);
        assert_eq!(svc.repo().list_by_project(&pid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn changed_spec_reapplies_in_place() {
        let (wf, reg, mut svc, pid) = harness().await;
        let applies = Arc::new(AtomicUsize::new(0));
        svc.register_backend(
            "terraform",
            Arc::new(BlockingProvisioner {
                gate: Arc::new(Semaphore::new(2)),
                applies: applies.clone(),
            }),
        );
        let first = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "upd",
                serde_json::json!({ "name": "upd", "versioning": false }),
                "user:default/a",
            )
            .await
            .unwrap();
        let id1 = first.provisioned.unwrap().id;
        assert_eq!(applies.load(Ordering::SeqCst), 1);

        // Re-request the same name with a changed spec: same record, re-applied
        // in place (no second row, no new id), and the new request fulfilled.
        let second = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "upd",
                serde_json::json!({ "name": "upd", "versioning": true }),
                "user:default/a",
            )
            .await
            .unwrap();
        let rec = second.provisioned.unwrap();
        assert_eq!(rec.id, id1);
        assert_eq!(rec.state, "provisioned");
        assert_eq!(rec.spec.get("versioning"), Some(&serde_json::json!(true)));
        assert_eq!(applies.load(Ordering::SeqCst), 2);
        assert_eq!(svc.repo().list_by_project(&pid).await.unwrap().len(), 1);
        assert_eq!(second.request.state, State::Fulfilled);
        assert_ne!(second.request.id, first.request.id);
    }

    #[tokio::test]
    async fn deploy_image_merges_only_image_and_preserves_spec() {
        let (wf, reg, svc, pid) = harness().await;
        // ecs-task is a non-long-running, image-bearing stand-in for any container
        // service; the stub connector applies inline.
        let rid = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ecs-task",
                "web",
                serde_json::json!({
                    "name": "web", "image": "repo:v1",
                    "env": { "FOO": "bar" }, "certificate_arn": "arn:acm:cert"
                }),
                "user:default/a",
            )
            .await
            .unwrap()
            .provisioned
            .unwrap()
            .id;

        let out = svc
            .deploy_image(&wf, &reg, &pid, &rid, "repo:v2", "user:default/a")
            .await
            .unwrap();

        let rec = out.provisioned.unwrap();
        assert_eq!(rec.id, rid); // same record, in place
        assert!(!out.pending_approval);
        assert_eq!(rec.spec["image"], serde_json::json!("repo:v2")); // swapped
        assert_eq!(rec.spec["env"], serde_json::json!({ "FOO": "bar" })); // preserved
        assert_eq!(
            rec.spec["certificate_arn"],
            serde_json::json!("arn:acm:cert")
        );
        assert_eq!(svc.repo().list_by_project(&pid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deploy_image_rejects_imageless_and_non_provisioned() {
        let (wf, reg, svc, pid) = harness().await;
        // No `image` field in the spec → rejected.
        let no_img = svc
            .request(
                &wf,
                &reg,
                &pid,
                "s3-bucket",
                "assets",
                serde_json::json!({ "name": "assets" }),
                "user:default/a",
            )
            .await
            .unwrap()
            .provisioned
            .unwrap()
            .id;
        let err = svc
            .deploy_image(&wf, &reg, &pid, &no_img, "x:v1", "user:default/a")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("image"), "got: {err}");

        // A live image-bearing resource that isn't `provisioned` → rejected.
        let now = asgard_storage::now();
        let failed = ProvisionedRecord {
            id: asgard_storage::new_uid(),
            project_id: pid.clone(),
            rtype: "ecs-task".into(),
            name: "broken".into(),
            spec: serde_json::json!({ "name": "broken", "image": "repo:v1" }),
            outputs: serde_json::json!({}),
            tags: Default::default(),
            est_monthly_usd: 5.0,
            state: "failed".into(),
            backend: "stub".into(),
            dry_run: true,
            request_id: None,
            created_at: now.clone(),
            updated_at: now,
            error: "boom".into(),
            attempts: 1,
            next_retry_at: None,
        };
        let rid = failed.id.clone();
        svc.repo().record(&failed).await.unwrap();
        let err = svc
            .deploy_image(&wf, &reg, &pid, &rid, "repo:v2", "user:default/a")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("provisioned"), "got: {err}");
    }

    #[tokio::test]
    async fn deploy_image_does_not_double_count_ceiling() {
        let (wf, reg, mut svc, pid) = harness().await;
        // Ceiling between one estimate (5) and two (10): an image bump must count
        // only the delta, not re-add the resource's own already-committed estimate,
        // or it would falsely trip review and block CD.
        svc.set_auto_approve(AutoApprovePolicy {
            ceilings: BTreeMap::from([("poc".into(), 8.0)]),
        });
        let rid = svc
            .request(
                &wf,
                &reg,
                &pid,
                "ecs-task",
                "web",
                serde_json::json!({ "name": "web", "image": "repo:v1" }),
                "user:default/a",
            )
            .await
            .unwrap()
            .provisioned
            .unwrap()
            .id;

        let out = svc
            .deploy_image(&wf, &reg, &pid, &rid, "repo:v2", "user:default/a")
            .await
            .unwrap();
        assert!(
            !out.pending_approval,
            "image bump must stay self-service (no double-count)"
        );
        assert_eq!(
            out.provisioned.unwrap().spec["image"],
            serde_json::json!("repo:v2")
        );
    }

    // ---- run-log capture + auto-retry ----

    /// A connector that fails its first `fails` applies then succeeds, recording a
    /// run entry (with a known output) for each — mirrors the real connectors'
    /// connector-sink capture so a test can assert both the run-log and the retry
    /// bookkeeping.
    struct FlakyProvisioner {
        fails: Arc<AtomicUsize>,
        applies: Arc<AtomicUsize>,
        run_log: Option<Arc<RunLogStore>>,
    }

    impl FlakyProvisioner {
        async fn record(&self, req: &ProvisionRequest, ok: bool, output: &str) {
            if let (Some(store), Some(rid)) = (&self.run_log, &req.resource_id) {
                let now = asgard_storage::now();
                let _ = store
                    .append(rid, &req.ctx.project_id, "apply", ok, output, &now, &now)
                    .await;
            }
        }
    }

    #[async_trait::async_trait]
    impl Provisioner for FlakyProvisioner {
        fn name(&self) -> &str {
            "flaky"
        }
        fn dry_run(&self) -> bool {
            true
        }
        fn supports(&self, _r: &str) -> bool {
            true
        }
        async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError> {
            Ok(Plan {
                summary: String::new(),
                tags: req.ctx.tags(),
                estimated_monthly_usd: req.estimated_monthly_usd,
            })
        }
        async fn apply(
            &self,
            req: &ProvisionRequest,
            _plan: &Plan,
        ) -> Result<Provisioned, ProvisionError> {
            self.applies.fetch_add(1, Ordering::SeqCst);
            if self.fails.load(Ordering::SeqCst) > 0 {
                self.fails.fetch_sub(1, Ordering::SeqCst);
                self.record(req, false, "boom: terraform apply failed")
                    .await;
                return Err(ProvisionError::Backend(
                    "boom: terraform apply failed".into(),
                ));
            }
            self.record(req, true, "Apply complete! Resources: 1 added")
                .await;
            Ok(Provisioned {
                outputs: serde_json::json!({ "ok": true }),
                resource_ids: vec![],
                sensitive_keys: vec![],
            })
        }
    }

    fn failed_row(pid: &str, name: &str, next_retry_at: Option<&str>) -> ProvisionedRecord {
        let mut rec = provisioning_row(pid, name);
        rec.state = "failed".into();
        rec.attempts = 1;
        rec.next_retry_at = next_retry_at.map(str::to_string);
        rec
    }

    #[tokio::test]
    async fn drive_core_captures_run_output_on_failure_and_success() {
        let (_wf, _reg, mut svc, pid) = harness().await;
        let store = Arc::new(RunLogStore::new(svc.repo().db().clone(), [0x33; 32]));
        svc.set_run_log(store.clone());
        svc.register_backend(
            "terraform",
            Arc::new(FlakyProvisioner {
                fails: Arc::new(AtomicUsize::new(1)),
                applies: Arc::new(AtomicUsize::new(0)),
                run_log: Some(store.clone()),
            }),
        );
        let rec = provisioning_row(&pid, "cap");
        svc.repo().record(&rec).await.unwrap();

        // First drive fails: captured run (ok=false) + a failed row with the retry
        // armed (attempts bumped, backoff deadline set).
        assert!(svc.drive_core(&rec.id).await.is_err());
        let runs = svc.resource_runs(&rec.id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].ok);
        assert!(runs[0].output.contains("boom"));
        let failed = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.attempts, 1);
        assert!(failed.next_retry_at.is_some());

        // Re-drive (the failed row maps to Apply): captured run (ok=true), the row
        // converges to provisioned, and `finish` resets the retry bookkeeping.
        svc.drive_core(&rec.id).await.unwrap();
        let runs = svc.resource_runs(&rec.id).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[1].ok);
        let done = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(done.state, "provisioned");
        assert_eq!(done.attempts, 0);
        assert!(done.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn list_retryable_respects_the_backoff_window() {
        let (_wf, _reg, svc, pid) = harness().await;
        let now = asgard_storage::now();
        let past = asgard_storage::plus_seconds(&now, -10);
        let future = asgard_storage::plus_seconds(&now, 600);
        let due = failed_row(&pid, "due", Some(&past));
        let pending = failed_row(&pid, "pending", Some(&future));
        let capped = failed_row(&pid, "capped", None);
        svc.repo().record(&due).await.unwrap();
        svc.repo().record(&pending).await.unwrap();
        svc.repo().record(&capped).await.unwrap();
        let rows = svc.repo().list_retryable(&now, &now, 50).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&due.id.as_str()));
        assert!(!ids.contains(&pending.id.as_str()));
        assert!(!ids.contains(&capped.id.as_str()));
    }

    #[tokio::test]
    async fn failed_apply_arms_backoff_then_caps_out_of_retryable() {
        let (_wf, _reg, mut svc, pid) = harness().await;
        svc.set_max_retries(2);
        svc.register_backend(
            "terraform",
            Arc::new(FlakyProvisioner {
                fails: Arc::new(AtomicUsize::new(usize::MAX)),
                applies: Arc::new(AtomicUsize::new(0)),
                run_log: None,
            }),
        );
        let rec = provisioning_row(&pid, "perma");
        svc.repo().record(&rec).await.unwrap();

        // Attempt 1 fails: under the cap, so a retry is armed.
        assert!(svc.drive_core(&rec.id).await.is_err());
        let r1 = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(r1.attempts, 1);
        assert!(r1.next_retry_at.is_some());

        // Attempt 2 fails and hits the cap: next_retry_at cleared, so it drops out of
        // the auto-retry sweep and rests as `failed`.
        assert!(svc.drive_core(&rec.id).await.is_err());
        let r2 = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(r2.attempts, 2);
        assert!(r2.next_retry_at.is_none());
        let now = asgard_storage::now();
        let rows = svc.repo().list_retryable(&now, &now, 50).await.unwrap();
        assert!(!rows.iter().any(|r| r.id == rec.id));
    }

    #[tokio::test]
    async fn per_service_retry_policy_overrides_the_default() {
        let (_wf, _reg, mut svc, pid) = harness().await;
        let dir = std::env::temp_dir().join(format!("asgard-svc-{}", asgard_storage::new_uid()));
        std::fs::create_dir_all(dir.join("zero-retry-svc")).unwrap();
        std::fs::write(
            dir.join("zero-retry-svc").join("service.yaml"),
            "id: zero-retry-svc\n\
             name: Zero Retry\n\
             category: tooling\n\
             auto_approvable: true\n\
             required_fields: [name]\n\
             provisioner:\n  connector: stub\n  config: {}\n\
             retry:\n  max_attempts: 0\n  base_secs: 5\n  cap_secs: 50\n\
             cost:\n  model: free\n  estimated_monthly_usd: 0.0\n  source:\n    type: none\n",
        )
        .unwrap();
        svc.set_catalog(ServiceCatalog::load(Some(&dir)).unwrap());

        // The manifest's retry block wins; a service with none inherits the defaults.
        let zero = svc.retry_policy_for("zero-retry-svc");
        assert_eq!(zero.max_attempts, 0);
        assert_eq!(zero.base_secs, 5);
        assert_eq!(zero.cap_secs, 50);
        let dflt = svc.retry_policy_for("s3-bucket");
        assert_eq!(dflt.max_attempts, MAX_RETRIES);
        assert_eq!(dflt.base_secs, RETRY_BASE_SECS);

        // Behaviorally: a failed apply for the zero-retry service arms no backoff, so
        // it never enters the auto-retry sweep.
        svc.register_backend(
            "stub",
            Arc::new(FlakyProvisioner {
                fails: Arc::new(AtomicUsize::new(usize::MAX)),
                applies: Arc::new(AtomicUsize::new(0)),
                run_log: None,
            }),
        );
        let mut rec = provisioning_row(&pid, "z");
        rec.rtype = "zero-retry-svc".into();
        svc.repo().record(&rec).await.unwrap();
        assert!(svc.drive_core(&rec.id).await.is_err());
        let got = svc.repo().get(&rec.id).await.unwrap().unwrap();
        assert_eq!(got.state, "failed");
        assert!(got.next_retry_at.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retry_resource_rearms_a_capped_failed_row() {
        let (_wf, _reg, mut svc, pid) = harness().await;
        svc.register_backend(
            "terraform",
            Arc::new(FlakyProvisioner {
                fails: Arc::new(AtomicUsize::new(0)),
                applies: Arc::new(AtomicUsize::new(0)),
                run_log: None,
            }),
        );
        // A row that auto-retry has given up on (next_retry_at = NULL).
        let mut rec = failed_row(&pid, "manual", None);
        rec.attempts = 5;
        svc.repo().record(&rec).await.unwrap();
        // Manual retry re-arms it in place and drives it to success.
        svc.retry_resource(&rec.id).await.unwrap();
        let done = await_state(&svc, &rec.id, "provisioned").await;
        assert_eq!(done.attempts, 0);
        assert!(done.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn retry_resource_unsticks_a_stranded_provisioning_row() {
        // A row stuck in `provisioning` (a crashed/OOM-killed worker, no live driver)
        // is the symptom of the worker-death bug. Manual retry must reclaim and drive
        // it — previously a no-op that left it for the slow reconcile sweep. Routes to
        // the stub connector (s3-bucket's `terraform` is unregistered here).
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "stranded");
        svc.repo().record(&rec).await.unwrap();
        let got = svc.retry_resource(&rec.id).await.unwrap();
        assert_eq!(got.state, "provisioned");
    }

    #[tokio::test]
    async fn retry_resource_does_not_yank_a_live_apply() {
        // The safety guarantee: a `provisioning` row a live worker holds (fresh claim)
        // must be left alone, so a manual retry can't trample a healthy in-flight apply.
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "live");
        svc.repo().record(&rec).await.unwrap();
        let past = asgard_storage::plus_seconds(&asgard_storage::now(), -600);
        assert!(svc
            .repo()
            .claim(&rec.id, "provisioning", "live-worker", &past)
            .await
            .unwrap());
        // No-op: still provisioning, and the claim is untouched (a non-stale reclaim
        // still loses, proving `live-worker` kept it).
        let got = svc.retry_resource(&rec.id).await.unwrap();
        assert_eq!(got.state, "provisioning");
        assert!(!svc
            .repo()
            .reclaim_stale(&rec.id, &stale_cutoff())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn reclaim_stale_loses_to_a_live_claim_and_wins_when_stale() {
        let (_wf, _reg, svc, pid) = harness().await;
        let rec = provisioning_row(&pid, "rc");
        svc.repo().record(&rec).await.unwrap();
        let now = asgard_storage::now();
        let past = asgard_storage::plus_seconds(&now, -600);
        let future = asgard_storage::plus_seconds(&now, 600);
        assert!(svc
            .repo()
            .claim(&rec.id, "provisioning", "owner-1", &past)
            .await
            .unwrap());
        // Cutoff in the past → the live claim is not stale → reclaim loses.
        assert!(!svc.repo().reclaim_stale(&rec.id, &past).await.unwrap());
        // Cutoff in the future → the claim counts as stale → reclaim wins.
        assert!(svc.repo().reclaim_stale(&rec.id, &future).await.unwrap());
        // The claim was cleared: a fresh claimant now wins even with a past cutoff.
        assert!(svc
            .repo()
            .claim(&rec.id, "provisioning", "owner-2", &past)
            .await
            .unwrap());
    }
}
