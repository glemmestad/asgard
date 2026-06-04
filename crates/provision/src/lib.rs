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
pub mod secrets;
mod stub;
mod tfstate;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use asgard_registry::{ProjectRegistry, Registration, RegistryError};
use asgard_storage::audit::{self, AuditRecord};
use asgard_storage::Db;
use asgard_workflow::{NewRequest, State, WorkflowEngine, WorkflowError, WorkflowRequest};

pub use connectors::{ExecConnector, LiteLlmConnector, TerraformConnector};
pub use cost::{
    build_tree, movers, tagged_report, AnomalyRow, AwsCostExplorerSource, CostNode, CostRollupRepo,
    CostSource, CostSourceRegistry, DatabricksCostSource, DimRow, ExecCostSource, FlatSource,
    ForecastRow, GatewaySource, LiteLlmCostSource, Mover, Movers, ProjectFact, ProjectOverlay,
    RollupDim, RollupRow, TaggedReport,
};
pub use manifest::{
    class_rank, InferenceCfg, InferenceModel, Resolved, ServiceCatalog, ServiceManifest, Variant,
    Variants,
};
pub use repo::{ProvisionRepo, ProvisionedRecord};
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

/// When a resource request may skip human approval. All conditions must hold:
/// the project's classification is in `classifications` (POC by default), the
/// resource's estimated monthly cost is within `max_resource_monthly_usd`, and
/// the project's total committed spend stays within `max_project_monthly_usd`.
#[derive(Debug, Clone)]
pub struct AutoApprovePolicy {
    pub classifications: Vec<String>,
    pub max_resource_monthly_usd: f64,
    pub max_project_monthly_usd: f64,
}

impl Default for AutoApprovePolicy {
    fn default() -> Self {
        AutoApprovePolicy {
            classifications: vec!["poc".to_string()],
            max_resource_monthly_usd: 50.0,
            max_project_monthly_usd: 500.0,
        }
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
}

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

        let cloud = spec
            .get("cloud")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_cloud)
            .to_string();
        let account = spec
            .get("account")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_account)
            .to_string();
        let target = CloudTarget {
            cloud: cloud.clone(),
            account: account.clone(),
        };
        if !self.allowed.contains(&target) {
            return Err(ProvisionError::NotPermitted(format!(
                "target {cloud}/{account} is not an allowed provisioning target"
            )));
        }

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
        let infra_so_far = self.repo.infra_total_for_project(project_id).await?;
        let class_ok = self
            .auto
            .classifications
            .iter()
            .any(|c| c == &reg.classification);
        let within_resource = est <= self.auto.max_resource_monthly_usd;
        let within_project =
            (reg.spent_usd + infra_so_far + est) <= self.auto.max_project_monthly_usd;
        let auto_ok = manifest.auto_approvable
            && !resolved.force_review
            && class_ok
            && within_resource
            && within_project;
        let tier_str = if auto_ok { "self_service" } else { "review" };
        let sla_seconds = if auto_ok { None } else { Some(7 * 24 * 3600) };

        // The payload doubles as the policy context, so it must stay Cedar-safe
        // (strings/ints/bools only — no floats, no arbitrary nested records). The
        // raw spec is carried as a JSON string and the cost estimate is derived
        // from the manifest at provision time rather than passed through here.
        let payload = serde_json::json!({
            "project_id": project_id,
            "resource_type": resource_type,
            "name": name,
            "spec_json": spec.to_string(),
            "data_class": reg.data_class,
            "provision_tier": tier_str,
            "cloud": cloud,
            "account": account,
        });
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
            let (request, record) = self.do_provision(workflow, &reg, req, requester).await?;
            Ok(RequestOutcome {
                request,
                provisioned: Some(record),
                pending_approval: false,
            })
        } else {
            let pending = req.state == State::Requested;
            Ok(RequestOutcome {
                request: req,
                provisioned: None,
                pending_approval: pending,
            })
        }
    }

    /// Tear down a provisioned resource (project decommission / cleanup). Routes
    /// to the manifest's connector and marks the record destroyed.
    pub async fn deprovision(
        &self,
        resource_id: &str,
        actor: &str,
    ) -> Result<ProvisionedRecord, ProvisionError> {
        let rec = self
            .repo
            .get(resource_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(resource_id.to_string()))?;
        let (backend, preq) = self.backend_and_req(&rec);
        backend.destroy(&preq, &rec.outputs).await?;
        self.repo.mark_destroyed(resource_id).await?;
        let _ = actor;
        let mut out = rec;
        out.state = "destroyed".into();
        Ok(out)
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
        let req = workflow
            .get(request_id)
            .await?
            .ok_or_else(|| ProvisionError::NotFound(request_id.to_string()))?;
        let project_id = req
            .payload
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProvisionError::InvalidSpec("request has no project_id".into()))?;
        let reg = registry.require_active(project_id).await?;
        let (request, record) = self.do_provision(workflow, &reg, req, actor).await?;
        Ok(RequestOutcome {
            request,
            provisioned: Some(record),
            pending_approval: false,
        })
    }

    /// The connector name + its config for a service id (empty config if the
    /// manifest is gone).
    fn connector_and_config(&self, resource_type: &str) -> (String, serde_json::Value) {
        match self.catalog.get(resource_type) {
            Some(m) => (m.connector().to_string(), m.connector_config()),
            None => ("stub".to_string(), serde_json::json!({})),
        }
    }

    async fn do_provision(
        &self,
        workflow: &WorkflowEngine,
        reg: &Registration,
        req: WorkflowRequest,
        actor: &str,
    ) -> Result<(WorkflowRequest, ProvisionedRecord), ProvisionError> {
        if req.state != State::Approved {
            return Err(ProvisionError::NotPermitted(format!(
                "request is {}, must be approved before provisioning",
                req.state.as_str()
            )));
        }
        // Idempotent retry: if a prior fulfill recorded the resource but failed to
        // transition the request, don't re-provision or write a duplicate row —
        // just (re)advance the workflow to Fulfilled.
        if let Some(existing) = self.repo.get_by_request(&req.id).await? {
            let fulfilled = workflow.fulfill(&req.id, actor).await?;
            return Ok((fulfilled, existing));
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

        let manifest = self.catalog.get(&resource_type);
        let connector = manifest
            .map(|m| m.connector().to_string())
            .unwrap_or_else(|| "stub".to_string());
        let config = manifest
            .map(|m| m.connector_config())
            .unwrap_or_else(|| serde_json::json!({}));
        // Record the resolved *variant* cost (e.g. the chosen instance size), not
        // the service's base estimate, so the rollup reflects what was provisioned.
        let est = manifest
            .map(|m| m.resolve(&spec).estimated_monthly_usd)
            .unwrap_or(0.0);
        let secret_outputs = manifest
            .map(|m| m.secret_outputs.clone())
            .unwrap_or_default();
        let backend = self.connector_backend(&connector);
        if !backend.supports(&resource_type) {
            return Err(ProvisionError::Unsupported(format!(
                "connector '{connector}' does not support '{resource_type}'"
            )));
        }

        let preq = ProvisionRequest {
            resource_type: resource_type.clone(),
            name: name.clone(),
            ctx: ResourceContext::from_registration(reg, &cloud, &account),
            spec: spec.clone(),
            config,
            estimated_monthly_usd: est,
            secret_outputs,
        };
        let plan = backend.plan(&preq).await?;
        let mut provisioned = backend.apply(&preq, &plan).await?;
        self.route_secrets(
            &reg.project_id,
            &name,
            &preq.secret_outputs,
            &mut provisioned,
        )
        .await?;

        let now = asgard_storage::now();
        let record = ProvisionedRecord {
            id: asgard_storage::new_uid(),
            project_id: reg.project_id.clone(),
            rtype: resource_type,
            name,
            spec,
            outputs: provisioned.outputs,
            tags: plan.tags,
            est_monthly_usd: plan.estimated_monthly_usd,
            state: "provisioned".into(),
            backend: backend.name().to_string(),
            dry_run: backend.dry_run(),
            request_id: Some(req.id.clone()),
            created_at: now.clone(),
            updated_at: now,
        };
        self.repo.record(&record).await?;
        let fulfilled = workflow.fulfill(&req.id, actor).await?;
        Ok((fulfilled, record))
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
        let svc = ProvisionService::new(ProvisionRepo::new(db.clone()));
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
        let (wf, reg, svc, pid) = harness().await;
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
        let done = svc
            .fulfill(&wf, &reg, &out.request.id, "system")
            .await
            .unwrap();
        assert_eq!(done.request.state, State::Fulfilled);
        assert_eq!(done.provisioned.unwrap().rtype, "rds-postgres");
    }

    #[tokio::test]
    async fn non_poc_classification_requires_approval() {
        let (wf, reg, svc, _pid) = harness().await;
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
            "above-POC classification must require approval even for a self-service type"
        );
    }

    #[tokio::test]
    async fn over_cost_limit_requires_approval() {
        let (wf, reg, mut svc, pid) = harness().await;
        svc.set_auto_approve(AutoApprovePolicy {
            classifications: vec!["poc".into()],
            max_resource_monthly_usd: 1.0, // s3 est is 5.0 → over the cap
            max_project_monthly_usd: 500.0,
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
        let (wf, reg, svc, pid) = harness().await; // poc
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
        assert!(!micro.pending_approval, "t3.micro ($7) is under the cap");
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
        assert!(big.pending_approval, "m7i.4xlarge ($500) is over the cap");
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
            classifications: vec!["wide-operational".into()],
            max_resource_monthly_usd: 100_000.0,
            max_project_monthly_usd: 100_000.0,
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
}
