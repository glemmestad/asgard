//! Project registration — the mandatory gate (brief: agents-first onboarding loop).
//!
//! Registration is the single record that BOTH (a) fails closed: no gateway key
//! is minted and no resource is provisioned for an unregistered or inactive
//! project, AND (b) supplies the cost-attribution dimensions (owner / manager /
//! group / cost-center) that get stamped onto every usage event. The runtime row
//! in `projects_runtime` is the system of record; a discoverable `Project`
//! catalog entity is projected from it.
//!
//! Identity is self-asserted: owner/manager are unverified emails (no OIDC), so
//! the owner/manager check is an attribution rule, not a security boundary. The
//! one controlled vocabulary is `group`, validated against an operator-configured
//! allowlist so cost-centers don't drift.

mod cost;
pub mod evidence;
pub mod governance;
pub mod guidance;
mod knowledge_seed;
pub mod mcp_servers;
pub mod promotion;
pub mod promotion_reviewer;
pub mod recipes;
pub mod review;
pub mod review_jobs;
pub mod skills;
pub mod standards;
pub mod versions;

pub use cost::{CostDim, CostReport, CostRow};
pub use evidence::Evidence;
pub use governance::{GovernanceConfig, GovernanceMetrics, Metric, PromotionSample};
pub use guidance::Guidance;
pub use mcp_servers::{McpServer, McpServerInput};
pub use promotion::{ClassificationRequirements, EvidenceVerdict, PromotionChecklist};
pub use promotion_reviewer::{ReviewerOutcome, ReviewerPanel};
pub use recipes::Recipe;
pub use review::{ExtendOutcome, ReviewConfig, ReviewState, SweepSummary};
pub use review_jobs::{ReviewJob, ReviewJobs};
pub use skills::{Skill, SkillInput};
pub use standards::Standard;
pub use versions::Version;

use asgard_catalog::{CatalogRepo, Entity, Lifecycle, Manifest, Metadata, Origin};
use asgard_gateway::GatewayRepo;
use asgard_storage::Db;
use asgard_workflow::{NewRequest, RequestFilter, State, WorkflowEngine, WorkflowRequest};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

pub const CLASSIFICATIONS: &[&str] = &[
    "poc",
    "light-operational",
    "wide-operational",
    "critical-path",
];
pub const DATA_CLASSES: &[&str] = &["public", "internal", "confidential", "restricted"];

/// Review-job worker tunables. A real code review (repo fetch + several model
/// calls) can run a while, so the lease is generous; a crashed run is reclaimed
/// only after it lapses. Three attempts before the promotion fails closed.
const REVIEW_LEASE_SECS: i64 = 600;
const REVIEW_MAX_ATTEMPTS: i64 = 3;

/// The state to restore a `Reviewing` promotion to on a clean verdict — the
/// pre-review Cedar decision stashed in the payload at enqueue. Defaults to
/// `Requested` (a human gate) if absent: the safe direction.
fn pre_review_state(req: &WorkflowRequest) -> State {
    req.payload
        .get("pre_review_state")
        .and_then(|v| v.as_str())
        .map(State::parse)
        .unwrap_or(State::Requested)
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid registration: {0}")]
    Validation(String),
    #[error("project '{0}' is not registered — register it before requesting resources")]
    NotRegistered(String),
    #[error("project '{0}' is not active (it has been decommissioned)")]
    Inactive(String),
    #[error("gateway: {0}")]
    Gateway(#[from] asgard_gateway::GatewayError),
    #[error("workflow: {0}")]
    Workflow(#[from] asgard_workflow::WorkflowError),
    #[error("catalog: {0}")]
    Catalog(#[from] asgard_catalog::CatalogError),
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// One operator-configured group / cost-center the deployer permits projects to
/// roll up to. `key` is the value supplied at registration; `cost_center` is the
/// finance code spend is attributed to (defaults to `key` when unset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupEntry {
    pub key: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub cost_center: String,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

impl GroupEntry {
    fn resolved_cost_center(&self) -> String {
        if self.cost_center.is_empty() {
            self.key.clone()
        } else {
            self.cost_center.clone()
        }
    }
}

/// The operator-configured set of valid groups. An empty allowlist means
/// "accept any group" (open mode) — registration still records the value but
/// performs no membership check. A non-empty allowlist is authoritative.
#[derive(Debug, Clone, Default)]
pub struct GroupAllowlist {
    entries: Vec<GroupEntry>,
}

impl GroupAllowlist {
    pub fn new(entries: Vec<GroupEntry>) -> Self {
        GroupAllowlist { entries }
    }
    pub fn is_open(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn entries(&self) -> &[GroupEntry] {
        &self.entries
    }
    pub fn active_keys(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.active)
            .map(|e| e.key.clone())
            .collect()
    }
    /// Resolve a supplied group to its allowlist entry. In open mode, synthesizes
    /// an entry whose cost-center mirrors the group key.
    fn resolve(&self, group: &str) -> Result<GroupEntry, RegistryError> {
        if self.is_open() {
            return Ok(GroupEntry {
                key: group.to_string(),
                display_name: group.to_string(),
                cost_center: group.to_string(),
                active: true,
            });
        }
        self.entries
            .iter()
            .find(|e| e.active && e.key == group)
            .cloned()
            .ok_or_else(|| {
                RegistryError::Validation(format!(
                    "group '{group}' is not an allowed cost-center; choose one of: {}",
                    self.active_keys().join(", ")
                ))
            })
    }
}

/// Operator-configured registration policy: which optional fields are required.
/// Defaults preserve the original strict posture (manager + group required); an
/// operator relaxes it in `asgard.yaml` so a solo founder can self-register.
#[derive(Debug, Clone)]
pub struct RegistrationPolicy {
    /// When false, `manager` may be blank and defaults to the owner (self-manage).
    pub require_manager: bool,
    /// When false, `group` may be blank (ungrouped, blank cost-center).
    pub require_group: bool,
}

impl Default for RegistrationPolicy {
    fn default() -> Self {
        RegistrationPolicy {
            require_manager: true,
            require_group: true,
        }
    }
}

/// What a caller supplies to register a project. `project_id` is server-minted,
/// never supplied here.
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterInput {
    pub name: String,
    pub owner_email: String,
    /// Optional per policy: blank defaults to the owner (self-manage).
    #[serde(default)]
    pub manager_email: String,
    /// Optional per policy: blank stores an ungrouped project (blank cost-center).
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub data_class: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(flatten)]
    pub evidence: Evidence,
}

/// A registered project record (the runtime row, serialized for surfaces).
#[derive(Debug, Clone, Serialize)]
pub struct Registration {
    pub project_id: String,
    pub name: String,
    pub owner: String,
    pub manager: String,
    pub group: String,
    pub cost_center: String,
    pub classification: String,
    pub data_class: String,
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub lifecycle: String,
    pub registered: bool,
    pub killed: bool,
    pub description: String,
    pub created_at: String,
    /// WS3 review-date engine: next review deadline (ISO-8601, blank = none),
    /// the expiry flag (`ok`/`expired`), and the automatic-extension count.
    pub review_date: String,
    pub review_state: String,
    pub review_extensions: i64,
    pub stack_exception_renewal_date: String,
    #[serde(flatten)]
    pub evidence: Evidence,
}

/// Post-registration mutable fields. The project id is never among them — it is
/// the stable handle every service tags and costs against. Each `None` leaves
/// the stored value untouched.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ProjectUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub budget_usd: Option<f64>,
}

/// What happened to a budget change: nothing requested, applied self-service, or
/// routed to human review because it exceeds the classification ceiling.
#[derive(Debug)]
pub enum BudgetOutcome {
    Unchanged,
    Applied,
    PendingReview(Box<WorkflowRequest>),
}

#[derive(Clone)]
pub struct ProjectRegistry {
    db: Db,
    gateway: GatewayRepo,
    catalog: CatalogRepo,
    allowlist: GroupAllowlist,
    policy: RegistrationPolicy,
    requirements: ClassificationRequirements,
    review: ReviewConfig,
    governance: GovernanceConfig,
    reviewer_panel: Option<Arc<dyn ReviewerPanel>>,
}

impl ProjectRegistry {
    pub fn new(
        db: Db,
        gateway: GatewayRepo,
        catalog: CatalogRepo,
        allowlist: GroupAllowlist,
        policy: RegistrationPolicy,
    ) -> Self {
        ProjectRegistry {
            db,
            gateway,
            catalog,
            allowlist,
            policy,
            requirements: ClassificationRequirements::default(),
            review: ReviewConfig::default(),
            governance: GovernanceConfig::default(),
            reviewer_panel: None,
        }
    }

    /// Override the per-tier evidence requirement table (operator config). The
    /// default ships the policy doc's table; mirrors `GroupAllowlist`'s
    /// default-then-override posture.
    pub fn with_requirements(mut self, requirements: ClassificationRequirements) -> Self {
        self.requirements = requirements;
        self
    }

    /// Override the review-date thresholds (POC window, automatic-extension
    /// count). Defaults are the policy doc's 90 days / 1 extension.
    pub fn with_review_config(mut self, review: ReviewConfig) -> Self {
        self.review = review;
        self
    }

    /// Override the governance metric thresholds (the two-maintainer minimum).
    /// Default is the policy doc's 2; mirrors `with_review_config`.
    pub fn with_governance_config(mut self, governance: GovernanceConfig) -> Self {
        self.governance = governance;
        self
    }

    /// Attach the machine-review panel run on `request_promotion`. Default `None`
    /// preserves the pre-review presence-check behavior exactly.
    pub fn with_reviewer_panel(mut self, panel: Arc<dyn ReviewerPanel>) -> Self {
        self.reviewer_panel = Some(panel);
        self
    }

    pub fn allowlist(&self) -> &GroupAllowlist {
        &self.allowlist
    }

    pub fn policy(&self) -> &RegistrationPolicy {
        &self.policy
    }

    pub fn requirements(&self) -> &ClassificationRequirements {
        &self.requirements
    }

    /// Whether `email` is an authority over `project_id` (its owner or manager),
    /// or the caller holds the see-all override. Mirrors the visibility rule used
    /// for cost/project scoping; reused to authorize mutations. `see_all` is the
    /// caller's `ViewAllCost`-equivalent (admin/finance) — they pass unconditionally.
    pub async fn is_authority(
        &self,
        project_id: &str,
        email: &str,
        see_all: bool,
    ) -> Result<bool, RegistryError> {
        if see_all {
            return Ok(true);
        }
        match self.get(project_id).await? {
            Some(r) => Ok(r.owner == email || r.manager == email),
            None => Ok(false),
        }
    }

    /// Register a project: validate, mint a stable id, write the runtime row
    /// (the gate + cost source of truth), and project a discoverable `Project`
    /// catalog entity. `actor` is the self-asserted caller, recorded for audit.
    pub async fn register(
        &self,
        input: RegisterInput,
        actor: &str,
    ) -> Result<Registration, RegistryError> {
        let name = input.name.trim();
        if name.is_empty() || is_placeholder(name) {
            return Err(RegistryError::Validation(
                "project name is required and must not be a template placeholder".into(),
            ));
        }
        let owner = normalize_email(&input.owner_email)?;
        // Manager is optional per policy: blank defaults to the owner (self-manage,
        // which a solo founder needs). owner == manager is always allowed.
        let manager_raw = input.manager_email.trim();
        let manager = if manager_raw.is_empty() {
            if self.policy.require_manager {
                return Err(RegistryError::Validation(
                    "manager_email is required".into(),
                ));
            }
            owner.clone()
        } else {
            normalize_email(manager_raw)?
        };

        // Group is optional per policy: blank stores an ungrouped project with a
        // blank cost-center; otherwise it must validate against the allowlist.
        let group_raw = input.group.trim();
        let group_entry = if group_raw.is_empty() {
            if self.policy.require_group {
                return Err(RegistryError::Validation("group is required".into()));
            }
            GroupEntry {
                key: String::new(),
                display_name: String::new(),
                cost_center: String::new(),
                active: true,
            }
        } else {
            self.allowlist.resolve(group_raw)?
        };

        let classification = input
            .classification
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("poc")
            .to_string();
        if !CLASSIFICATIONS.contains(&classification.as_str()) {
            return Err(RegistryError::Validation(format!(
                "classification must be one of: {}",
                CLASSIFICATIONS.join(", ")
            )));
        }
        let data_class = input
            .data_class
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("internal")
            .to_string();
        if !DATA_CLASSES.contains(&data_class.as_str()) {
            return Err(RegistryError::Validation(format!(
                "data_class must be one of: {}",
                DATA_CLASSES.join(", ")
            )));
        }
        let budget_usd = input.budget_usd.unwrap_or(0.0);
        if budget_usd < 0.0 {
            return Err(RegistryError::Validation("budget_usd must be >= 0".into()));
        }
        input.evidence.validate()?;
        let description = input.description.unwrap_or_default();

        let project_id = self.mint_project_id().await?;
        let cost_center = group_entry.resolved_cost_center();

        self.gateway
            .set_registration(
                &project_id,
                name,
                &description,
                &owner,
                &manager,
                &group_entry.key,
                &cost_center,
                &classification,
                &data_class,
                budget_usd,
            )
            .await?;

        evidence::write(&self.db, &project_id, &input.evidence).await?;

        let (_, _, created_at) = self.runtime_meta(&project_id).await?;
        review::set_initial(
            &self.db,
            &project_id,
            &created_at,
            &classification,
            &input.evidence.recurring_review_date,
            self.review.poc_window_days,
        )
        .await?;

        self.project_entity(
            &project_id,
            name,
            &description,
            &owner,
            &manager,
            &group_entry.key,
            &cost_center,
            &classification,
            budget_usd,
            "active",
        )
        .await?;

        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.registered")
                .entity(format!("project:{project_id}"))
                .outcome("registered")
                .data(serde_json::json!({
                    "owner": owner, "manager": manager, "group": group_entry.key,
                    "cost_center": cost_center, "classification": classification,
                })),
        )
        .await;

        self.get(&project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.clone()))
    }

    pub async fn get(&self, project_id: &str) -> Result<Option<Registration>, RegistryError> {
        let rt = match self.gateway.get_project(project_id).await? {
            Some(rt) => rt,
            None => return Ok(None),
        };
        let (display_name, description, created_at) = self.runtime_meta(project_id).await?;
        let evidence = evidence::read(&self.db, project_id).await?;
        let rv = review::read(&self.db, project_id).await?;
        Ok(Some(Registration {
            project_id: rt.project_id,
            name: display_name,
            owner: rt.owner,
            manager: rt.manager,
            group: rt.cost_group,
            cost_center: rt.cost_center,
            classification: rt.classification,
            data_class: rt.data_class,
            budget_usd: rt.budget_usd,
            spent_usd: rt.spent_usd,
            lifecycle: rt.lifecycle,
            registered: rt.registered,
            killed: rt.killed,
            description,
            created_at,
            review_date: rv.review_date,
            review_state: rv.review_state,
            review_extensions: rv.review_extensions,
            stack_exception_renewal_date: rv.stack_exception_renewal_date,
            evidence,
        }))
    }

    pub async fn list(&self) -> Result<Vec<Registration>, RegistryError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT project_id FROM projects_runtime WHERE registered = 1 ORDER BY project_id",
        )
        .fetch_all(self.db.pool())
        .await?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(r) = self.get(&id).await? {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// The gate: returns the registration only if the project is registered and
    /// active. Used before minting keys and before provisioning resources.
    pub async fn require_active(&self, project_id: &str) -> Result<Registration, RegistryError> {
        match self.get(project_id).await? {
            None => Err(RegistryError::NotRegistered(project_id.to_string())),
            Some(r) if !r.registered => Err(RegistryError::NotRegistered(project_id.to_string())),
            Some(r) if r.lifecycle != "active" => {
                Err(RegistryError::Inactive(project_id.to_string()))
            }
            Some(r) => Ok(r),
        }
    }

    pub async fn decommission(
        &self,
        project_id: &str,
        actor: &str,
        reason: &str,
    ) -> Result<Registration, RegistryError> {
        let _ = self.require_active(project_id).await?;
        self.gateway
            .set_lifecycle(project_id, "decommissioned")
            .await?;
        if let Ok(Some(e)) = self.catalog.get("Project", "default", project_id).await {
            let _ = self
                .catalog
                .set_lifecycle(&e.uid, Lifecycle::Decommissioned)
                .await;
        }
        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.decommissioned")
                .entity(format!("project:{project_id}"))
                .outcome("decommissioned")
                .reason(reason),
        )
        .await;
        self.get(project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.to_string()))
    }

    /// Replace the evidence record for a project (PUT semantics — the caller
    /// submits the full evidence block). Authority is the caller's responsibility,
    /// mirroring `decommission`. Records a `project.updated` audit event.
    pub async fn update_evidence(
        &self,
        project_id: &str,
        evidence: Evidence,
        actor: &str,
    ) -> Result<Registration, RegistryError> {
        if self.get(project_id).await?.is_none() {
            return Err(RegistryError::NotRegistered(project_id.to_string()));
        }
        evidence.validate()?;
        evidence::write(&self.db, project_id, &evidence).await?;
        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.updated")
                .entity(format!("project:{project_id}"))
                .outcome("updated"),
        )
        .await;
        self.get(project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.to_string()))
    }

    /// Mutable project fields that change without re-registering — the project id
    /// stays the stable handle every service tags and costs against, so a
    /// code-name POC can become a production name with no orphaning. `None`
    /// leaves a field untouched.
    pub async fn update_project(
        &self,
        workflow: &WorkflowEngine,
        project_id: &str,
        update: ProjectUpdate,
        ceiling: Option<f64>,
        actor: &str,
    ) -> Result<(Registration, BudgetOutcome), RegistryError> {
        let reg = self.require_active(project_id).await?;

        // Identity (name/description): a plain relabel — the id is unchanged.
        let new_name = update
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let new_desc = update.description.as_deref();
        if new_name.is_some() || new_desc.is_some() {
            let name = new_name.unwrap_or(&reg.name);
            let desc = new_desc.unwrap_or(&reg.description);
            self.gateway.set_identity(project_id, name, desc).await?;
            self.project_entity(
                project_id,
                name,
                desc,
                &reg.owner,
                &reg.manager,
                &reg.group,
                &reg.cost_center,
                &reg.classification,
                reg.budget_usd,
                &reg.lifecycle,
            )
            .await?;
            let _ = asgard_storage::audit::append(
                &self.db,
                &asgard_storage::audit::AuditRecord::new(actor, "project.updated")
                    .entity(format!("project:{project_id}"))
                    .outcome("updated"),
            )
            .await;
        }

        // Budget: self-service up to the classification's auto-approve ceiling. A
        // raise above it routes to human review (and applies on fulfill); lowering
        // is always allowed.
        let budget = match update.budget_usd {
            None => BudgetOutcome::Unchanged,
            Some(req) if req < 0.0 => {
                return Err(RegistryError::Validation("budget_usd must be >= 0".into()))
            }
            Some(req) if req <= reg.budget_usd || ceiling.is_some_and(|c| req <= c) => {
                self.gateway.set_budget(project_id, req).await?;
                let _ = asgard_storage::audit::append(
                    &self.db,
                    &asgard_storage::audit::AuditRecord::new(actor, "project.budget_set")
                        .entity(format!("project:{project_id}"))
                        .outcome("applied")
                        .data(serde_json::json!({ "budget_usd": req })),
                )
                .await;
                BudgetOutcome::Applied
            }
            Some(req) => {
                let request = workflow
                    .submit(NewRequest {
                        kind: "budget".into(),
                        requester: actor.to_string(),
                        subject: format!("budget/{project_id}"),
                        // The payload doubles as the Cedar context, which is
                        // float-hostile — carry the amounts as strings.
                        payload: serde_json::json!({
                            "project_id": project_id,
                            "requested_budget": req.to_string(),
                            "current_budget": reg.budget_usd.to_string(),
                        }),
                        sla_seconds: Some(7 * 24 * 3600),
                    })
                    .await?;
                BudgetOutcome::PendingReview(Box::new(request))
            }
        };

        let reg = self
            .get(project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.to_string()))?;
        Ok((reg, budget))
    }

    /// Apply an approved over-ceiling budget request. Mirrors `fulfill_promotion`:
    /// requires the request to be approved, then sets the budget and advances the
    /// workflow to `Fulfilled`.
    pub async fn fulfill_budget(
        &self,
        workflow: &WorkflowEngine,
        request_id: &str,
        actor: &str,
    ) -> Result<WorkflowRequest, RegistryError> {
        let req = workflow
            .get(request_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(request_id.to_string()))?;
        if req.state != State::Approved {
            return Err(RegistryError::Validation(format!(
                "request is {}, must be approved before applying",
                req.state.as_str()
            )));
        }
        let project_id = req
            .payload
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RegistryError::Validation("request has no project_id".into()))?;
        let amount = req
            .payload
            .get("requested_budget")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| RegistryError::Validation("request has no requested_budget".into()))?;
        self.gateway.set_budget(project_id, amount).await?;
        let fulfilled = workflow.fulfill(request_id, actor).await?;
        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.budget_set")
                .entity(format!("project:{project_id}"))
                .outcome("applied")
                .data(serde_json::json!({ "budget_usd": amount, "via": "review" })),
        )
        .await;
        Ok(fulfilled)
    }

    /// The self-serve promotion checklist: the project's current tier, the one
    /// tier it may move to, and the evidence verdict for that move (so an owner
    /// can close the gaps before requesting). `None`s mean it's at the top tier.
    pub async fn promotion_checklist(
        &self,
        project_id: &str,
    ) -> Result<PromotionChecklist, RegistryError> {
        let reg = self
            .get(project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.to_string()))?;
        let next = promotion::next_tier(&reg.classification).map(str::to_string);
        let verdict = next
            .as_deref()
            .map(|t| promotion::evaluate(&reg, t, &self.requirements));
        Ok(PromotionChecklist {
            current: reg.classification,
            next_tier: next,
            verdict,
        })
    }

    /// Request a one-step promotion. Validates the step, evaluates the evidence,
    /// and submits a `promotion` workflow request whose payload carries the policy
    /// facts; Cedar then auto-approves (Light, clean) or routes to a human.
    ///
    /// When an async reviewer (`code-review`) applies to `target`, the panel is
    /// deferred to the background worker: the request parks in `Reviewing` and a
    /// `review_jobs` row is enqueued — the call returns immediately. Otherwise the
    /// inline panel (synchronous reviewers) runs now and the request resolves to
    /// `Approved` / `Requested` / `Flagged` before returning.
    pub async fn request_promotion(
        &self,
        workflow: &WorkflowEngine,
        project_id: &str,
        target: &str,
        actor: &str,
    ) -> Result<WorkflowRequest, RegistryError> {
        let reg = self.require_active(project_id).await?;
        promotion::validate_step(&reg.classification, target)?;
        let verdict = promotion::evaluate(&reg, target, &self.requirements);
        let risk_accepted = !reg.evidence.risk_acceptance_url.trim().is_empty();
        let machine_has_exception = !verdict.exception_signals.is_empty();
        let async_review = self
            .reviewer_panel
            .as_ref()
            .map(|p| p.has_async(target))
            .unwrap_or(false);

        let subject = format!("project:{project_id}");
        // Re-running supersedes any open attempt (requested/flagged/reviewing)
        // rather than piling up duplicates; human decisions are left alone.
        for open in self.open_promotions(workflow, &subject).await? {
            let _ = workflow.cancel(&open.id, actor).await;
        }

        if async_review {
            // Submit on the machine verdict alone (the reviewers run later in the
            // worker), so Cedar gives the pre-review resting state. Park in
            // `Reviewing` and enqueue; the worker runs the panel and finalizes.
            let mut payload = serde_json::json!({
                "project_id": project_id,
                "from_classification": reg.classification,
                "target_classification": target,
                "evidence_complete": verdict.evidence_complete,
                "has_exception": machine_has_exception,
                "is_critical": target == "critical-path",
                "risk_accepted": risk_accepted,
                "missing": verdict.missing,
                "exception_signals": verdict.exception_signals,
            });
            let req = workflow
                .submit(NewRequest {
                    kind: "promotion".into(),
                    requester: actor.to_string(),
                    subject: subject.clone(),
                    payload: payload.clone(),
                    sla_seconds: Some(7 * 24 * 3600),
                })
                .await?;
            // A policy forbid is terminal — don't review a denied promotion.
            if req.state == State::Rejected {
                return Ok(req);
            }
            payload["pre_review_state"] = serde_json::json!(req.state.as_str());
            workflow.update_payload(&req.id, payload).await?;
            let reviewing = workflow.review(&req.id, actor).await?;
            self.jobs().enqueue(&req.id, project_id, target).await?;
            return Ok(reviewing);
        }

        // Inline path: run the panel now (synchronous reviewers only) and fold its
        // escalate-only signals into the Cedar decision before submitting.
        let outcome = match &self.reviewer_panel {
            Some(p) => p.review(&reg, target, &verdict).await,
            None => ReviewerOutcome::empty(),
        };
        let mut exception_signals = verdict.exception_signals.clone();
        exception_signals.extend(outcome.added_exception_signals.iter().cloned());
        let has_exception = !exception_signals.is_empty();

        let payload = serde_json::json!({
            "project_id": project_id,
            "from_classification": reg.classification,
            "target_classification": target,
            "evidence_complete": verdict.evidence_complete,
            "has_exception": has_exception,
            "is_critical": target == "critical-path",
            "risk_accepted": risk_accepted,
            "missing": verdict.missing,
            "exception_signals": exception_signals,
            "review_passed": outcome.passed,
            "review_findings": outcome.findings,
            "review_summary": outcome.summary,
            "reviewers_ran": outcome.reviewer_ids,
        });
        let req = workflow
            .submit(NewRequest {
                kind: "promotion".into(),
                requester: actor.to_string(),
                subject: subject.clone(),
                payload,
                sla_seconds: Some(7 * 24 * 3600),
            })
            .await?;

        let clean_state = req.state;
        self.finalize_promotion(
            workflow,
            req,
            project_id,
            target,
            clean_state,
            &outcome,
            has_exception,
            actor,
        )
        .await
    }

    /// Route a promotion to its resting state once its review outcome is known.
    /// Shared by the inline path (req fresh from `submit`, `clean_state` = its
    /// post-submit state) and the async worker (req parked in `Reviewing`,
    /// `clean_state` = the stashed pre-review state). Persists every verdict for
    /// audit. Escalate-only: an exception can only push to `Flagged`; a clean
    /// verdict restores `clean_state`. A `Rejected` / superseded request is
    /// terminal and left as-is.
    #[allow(clippy::too_many_arguments)]
    pub async fn finalize_promotion(
        &self,
        workflow: &WorkflowEngine,
        req: WorkflowRequest,
        project_id: &str,
        target: &str,
        clean_state: State,
        outcome: &ReviewerOutcome,
        has_exception: bool,
        actor: &str,
    ) -> Result<WorkflowRequest, RegistryError> {
        self.persist_reviews(&req.id, project_id, outcome).await;
        if matches!(
            req.state,
            State::Rejected | State::Cancelled | State::Fulfilled
        ) {
            return Ok(req);
        }

        let mut req = req;
        if has_exception {
            if req.state != State::Flagged {
                let summary = if outcome.summary.trim().is_empty() {
                    "promotion blocked by review findings".to_string()
                } else {
                    outcome.summary.clone()
                };
                req = workflow.flag(&req.id, actor, &summary).await?;
            }
            let _ = asgard_storage::audit::append(
                &self.db,
                &asgard_storage::audit::AuditRecord::new(actor, "project.promotion_flagged")
                    .entity(format!("project:{project_id}"))
                    .outcome("flagged")
                    .reason(req.reason.clone().unwrap_or_default())
                    .data(serde_json::json!({
                        "request_id": req.id,
                        "target": target,
                        "findings": outcome.findings,
                        "reviewers": outcome.reviewer_ids,
                    })),
            )
            .await;
        } else if req.state != clean_state {
            req = workflow
                .resolve_review(&req.id, clean_state, "review passed; no findings", actor)
                .await?;
        }
        Ok(req)
    }

    /// The async review-job queue (a thin handle over the shared `Db`).
    pub fn jobs(&self) -> review_jobs::ReviewJobs {
        review_jobs::ReviewJobs::new(self.db.clone())
    }

    /// Execute one claimed review job: load the promotion + project, re-evaluate
    /// the machine verdict, run the deferred reviewer panel, mirror its summary
    /// into the payload, and finalize. A superseded/terminal request is a no-op.
    /// Infra errors propagate so the worker can retry / fail the job closed.
    pub async fn run_review_job(
        &self,
        workflow: &WorkflowEngine,
        job: &ReviewJob,
    ) -> Result<WorkflowRequest, RegistryError> {
        let mut req = workflow
            .get(&job.request_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(job.request_id.clone()))?;
        if req.state != State::Reviewing {
            return Ok(req); // superseded or already resolved
        }
        let reg = self.require_active(&job.project_id).await?;
        let verdict = promotion::evaluate(&reg, &job.target, &self.requirements);
        let machine_has_exception = !verdict.exception_signals.is_empty();
        // The evidence machine already flags this — don't spend a deep code review
        // (repo fetch + model calls) on a promotion that fails the evidence bar.
        let outcome = if machine_has_exception {
            ReviewerOutcome::empty()
        } else {
            match &self.reviewer_panel {
                Some(p) => p.review(&reg, &job.target, &verdict).await,
                None => ReviewerOutcome::empty(),
            }
        };
        let has_exception = machine_has_exception || !outcome.added_exception_signals.is_empty();
        let clean_state = pre_review_state(&req);

        // Mirror the reviewer summary into the payload (parity with the inline path).
        if let serde_json::Value::Object(map) = &mut req.payload {
            map.insert("review_passed".into(), serde_json::json!(outcome.passed));
            map.insert(
                "review_findings".into(),
                serde_json::json!(outcome.findings),
            );
            map.insert("review_summary".into(), serde_json::json!(outcome.summary));
            map.insert(
                "reviewers_ran".into(),
                serde_json::json!(outcome.reviewer_ids),
            );
        }
        let _ = workflow.update_payload(&req.id, req.payload.clone()).await;

        self.finalize_promotion(
            workflow,
            req,
            &job.project_id,
            &job.target,
            clean_state,
            &outcome,
            has_exception,
            "system",
        )
        .await
    }

    /// Fail a stuck review closed: the worker exhausted its retries, so flag the
    /// promotion rather than leave it hanging in `Reviewing`. Escalate-only.
    async fn fail_review_closed(
        &self,
        workflow: &WorkflowEngine,
        job: &ReviewJob,
        error: &str,
    ) -> Result<(), RegistryError> {
        let Some(req) = workflow.get(&job.request_id).await? else {
            return Ok(());
        };
        if req.state != State::Reviewing {
            return Ok(());
        }
        let clean_state = pre_review_state(&req);
        let outcome = ReviewerOutcome {
            passed: false,
            added_exception_signals: vec![format!(
                "automated code review did not complete: {error}"
            )],
            findings: vec![format!(
                "code review failed to complete after retries: {error}"
            )],
            summary: "code review unavailable — fix and retry, or escalate".into(),
            reviewer_ids: vec!["code-review".into()],
            verdicts_json: vec![],
        };
        self.finalize_promotion(
            workflow,
            req,
            &job.project_id,
            &job.target,
            clean_state,
            &outcome,
            true,
            "system",
        )
        .await?;
        Ok(())
    }

    /// One worker pass over the review queue: reclaim stale leases, then run each
    /// pending job to completion (finalize + mark `done`, or `fail` — terminally
    /// failing closed to `Flagged`). Returns how many jobs it finalized. Idempotent
    /// and crash-safe; the background loop in `serve` calls it on a schedule and an
    /// admin can trigger it on demand.
    pub async fn drain_reviews(&self, workflow: &WorkflowEngine) -> Result<usize, RegistryError> {
        let jobs = self.jobs();
        jobs.reclaim_stale().await?;
        let mut done = 0;
        while let Some(job) = jobs.claim_next(REVIEW_LEASE_SECS).await? {
            match self.run_review_job(workflow, &job).await {
                Ok(_) => {
                    jobs.finish(&job.id).await?;
                    done += 1;
                }
                Err(e) => {
                    let terminal = jobs
                        .fail(&job.id, &e.to_string(), job.attempts, REVIEW_MAX_ATTEMPTS)
                        .await?;
                    if terminal {
                        let _ = self
                            .fail_review_closed(workflow, &job, &e.to_string())
                            .await;
                        done += 1;
                    } else {
                        // Re-queued for a later pass; stop now so we don't
                        // immediately re-claim the same row in this pass.
                        break;
                    }
                }
            }
        }
        Ok(done)
    }

    /// Open (`Requested`/`Flagged`/`Reviewing`) promotion requests for a subject.
    async fn open_promotions(
        &self,
        workflow: &WorkflowEngine,
        subject: &str,
    ) -> Result<Vec<WorkflowRequest>, RegistryError> {
        let all = workflow
            .list(&RequestFilter {
                subject: Some(subject.to_string()),
                ..Default::default()
            })
            .await?;
        Ok(all
            .into_iter()
            .filter(|r| {
                r.kind == "promotion"
                    && matches!(
                        r.state,
                        State::Requested | State::Flagged | State::Reviewing
                    )
            })
            .collect())
    }

    /// The reviewer verdicts recorded for a request (every attempt), newest-attempt
    /// rows last. Surfaced to the submitter and admins.
    pub async fn promotion_reviews(
        &self,
        request_id: &str,
    ) -> Result<Vec<serde_json::Value>, RegistryError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT reviewer_id, kind, disposition, verdict_json, model, cost_usd, created_at \
             FROM promotion_reviews WHERE request_id = ? ORDER BY created_at",
        ))
        .bind(request_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let vj: String = r.get("verdict_json");
                serde_json::json!({
                    "reviewer_id": r.get::<String, _>("reviewer_id"),
                    "kind": r.get::<String, _>("kind"),
                    "disposition": r.get::<String, _>("disposition"),
                    "model": r.get::<String, _>("model"),
                    "cost_usd": r.get::<f64, _>("cost_usd"),
                    "created_at": r.get::<String, _>("created_at"),
                    "verdict": serde_json::from_str::<serde_json::Value>(&vj)
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect())
    }

    /// Persist one row per reviewer verdict for audit (best-effort; a write
    /// failure must not fail the promotion itself).
    async fn persist_reviews(&self, request_id: &str, project_id: &str, outcome: &ReviewerOutcome) {
        for v in &outcome.verdicts_json {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let cost = v.get("cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let res = sqlx::query(&self.db.q(
                "INSERT INTO promotion_reviews (id, request_id, project_id, reviewer_id, kind, disposition, verdict_json, model, cost_usd, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            ))
            .bind(asgard_storage::new_uid())
            .bind(request_id)
            .bind(project_id)
            .bind(s("reviewer_id"))
            .bind(s("kind"))
            .bind(s("disposition"))
            .bind(v.to_string())
            .bind(s("model"))
            .bind(cost)
            .bind(asgard_storage::now())
            .execute(self.db.pool())
            .await;
            if let Err(e) = res {
                tracing::warn!("persist promotion review failed: {e}");
            }
        }
    }

    /// Fulfill an approved promotion: mutate `classification`, clear the now-met
    /// `requested_classification`, re-project the catalog entity, advance the
    /// workflow to `Fulfilled`, and audit `project.promoted` with from/to.
    pub async fn fulfill_promotion(
        &self,
        workflow: &WorkflowEngine,
        request_id: &str,
        actor: &str,
    ) -> Result<WorkflowRequest, RegistryError> {
        let req = workflow
            .get(request_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(request_id.to_string()))?;
        if req.state != State::Approved {
            return Err(RegistryError::Validation(format!(
                "request is {}, must be approved before promotion",
                req.state.as_str()
            )));
        }
        let project_id = req
            .payload
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RegistryError::Validation("request has no project_id".into()))?;
        let target = req
            .payload
            .get("target_classification")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RegistryError::Validation("request has no target_classification".into())
            })?
            .to_string();
        let reg = self.require_active(project_id).await?;
        let from = reg.classification.clone();

        self.gateway.set_classification(project_id, &target).await?;
        if !reg.evidence.requested_classification.is_empty() {
            let mut ev = reg.evidence.clone();
            ev.requested_classification = String::new();
            evidence::write(&self.db, project_id, &ev).await?;
        }
        self.project_entity(
            project_id,
            &reg.name,
            &reg.description,
            &reg.owner,
            &reg.manager,
            &reg.group,
            &reg.cost_center,
            &target,
            reg.budget_usd,
            &reg.lifecycle,
        )
        .await?;

        let fulfilled = workflow.fulfill(request_id, actor).await?;
        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.promoted")
                .entity(format!("project:{project_id}"))
                .outcome("promoted")
                .data(serde_json::json!({"from": from, "to": target})),
        )
        .await;
        Ok(fulfilled)
    }

    /// Demote a project down the ladder. Always explicit, never automatic; a
    /// reason is mandatory and the move must be strictly downward. Audited
    /// `project.demoted`.
    pub async fn demote(
        &self,
        project_id: &str,
        target: &str,
        actor: &str,
        reason: &str,
    ) -> Result<Registration, RegistryError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(RegistryError::Validation(
                "a reason is required to demote a project".into(),
            ));
        }
        let reg = self.require_active(project_id).await?;
        let cur = promotion::tier_rank(&reg.classification).ok_or_else(|| {
            RegistryError::Validation(format!(
                "project has an unknown classification '{}'",
                reg.classification
            ))
        })?;
        let tgt = promotion::tier_rank(target).ok_or_else(|| {
            RegistryError::Validation(format!("unknown classification '{target}'"))
        })?;
        if tgt >= cur {
            return Err(RegistryError::Validation(format!(
                "demotion target '{target}' must be below the current tier '{}'",
                reg.classification
            )));
        }
        let from = reg.classification.clone();
        self.gateway.set_classification(project_id, target).await?;
        self.project_entity(
            project_id,
            &reg.name,
            &reg.description,
            &reg.owner,
            &reg.manager,
            &reg.group,
            &reg.cost_center,
            target,
            reg.budget_usd,
            &reg.lifecycle,
        )
        .await?;
        let _ = asgard_storage::audit::append(
            &self.db,
            &asgard_storage::audit::AuditRecord::new(actor, "project.demoted")
                .entity(format!("project:{project_id}"))
                .outcome("demoted")
                .reason(reason)
                .data(serde_json::json!({"from": from, "to": target})),
        )
        .await;
        self.get(project_id)
            .await?
            .ok_or_else(|| RegistryError::NotRegistered(project_id.to_string()))
    }

    /// Sweep for overdue reviews and lapsed stack exceptions (WS3). Flips
    /// `review_state` ok→expired for projects past their `review_date` (auditing
    /// `project.review_expired` once per transition, so re-running is idempotent),
    /// and surfaces stack exceptions with no future renewal date. Expiry is a
    /// flag — it blocks nothing; lifecycle stays active.
    pub async fn sweep(&self, actor: &str) -> Result<SweepSummary, RegistryError> {
        let now = asgard_storage::now();
        let checked: i64 =
            sqlx::query_scalar(&self.db.q(
                "SELECT COUNT(*) FROM projects_runtime WHERE registered = 1 AND review_date != ''",
            ))
            .fetch_one(self.db.pool())
            .await?;

        let newly_expired: Vec<String> = sqlx::query_scalar(&self.db.q(
            "SELECT project_id FROM projects_runtime \
             WHERE registered = 1 AND review_state = 'ok' AND review_date != '' AND review_date < ?",
        ))
        .bind(&now)
        .fetch_all(self.db.pool())
        .await?;
        for id in &newly_expired {
            sqlx::query(&self.db.q(
                "UPDATE projects_runtime SET review_state = 'expired', updated_at = ? WHERE project_id = ?",
            ))
            .bind(&now)
            .bind(id)
            .execute(self.db.pool())
            .await?;
            let _ = asgard_storage::audit::append(
                &self.db,
                &asgard_storage::audit::AuditRecord::new(actor, "project.review_expired")
                    .entity(format!("project:{id}"))
                    .outcome("expired"),
            )
            .await;
        }

        let expired_exceptions: Vec<String> =
            sqlx::query_scalar(&self.db.q("SELECT project_id FROM projects_runtime \
             WHERE registered = 1 AND stack_exception != '' \
             AND (stack_exception_renewal_date = '' OR stack_exception_renewal_date < ?)"))
            .bind(&now)
            .fetch_all(self.db.pool())
            .await?;
        for id in &expired_exceptions {
            let _ = asgard_storage::audit::append(
                &self.db,
                &asgard_storage::audit::AuditRecord::new(actor, "project.exception_expired")
                    .entity(format!("project:{id}"))
                    .outcome("exception_expired"),
            )
            .await;
        }
        Ok(SweepSummary {
            checked,
            newly_expired,
            expired_exceptions,
        })
    }

    /// Extend a project's review deadline. Grants one automatic window while the
    /// project is under its extension allowance (clearing the expiry flag,
    /// auditing `project.review_extended`); beyond that, anything further is a
    /// human decision — a `review-extension` workflow request is created.
    pub async fn extend_review(
        &self,
        workflow: &WorkflowEngine,
        project_id: &str,
        actor: &str,
    ) -> Result<ExtendOutcome, RegistryError> {
        let _ = self.require_active(project_id).await?;
        let rv = review::read(&self.db, project_id).await?;
        if rv.review_extensions < self.review.auto_extensions {
            let base = if rv.review_date.is_empty() {
                asgard_storage::now()
            } else {
                rv.review_date.clone()
            };
            let new_date = asgard_storage::plus_days(&base, self.review.poc_window_days);
            sqlx::query(&self.db.q(
                "UPDATE projects_runtime SET review_date = ?, review_extensions = review_extensions + 1, \
                 review_state = 'ok', updated_at = ? WHERE project_id = ?",
            ))
            .bind(&new_date)
            .bind(asgard_storage::now())
            .bind(project_id)
            .execute(self.db.pool())
            .await?;
            let _ = asgard_storage::audit::append(
                &self.db,
                &asgard_storage::audit::AuditRecord::new(actor, "project.review_extended")
                    .entity(format!("project:{project_id}"))
                    .outcome("extended")
                    .data(serde_json::json!({"review_date": new_date})),
            )
            .await;
            let updated = review::read(&self.db, project_id).await?;
            Ok(ExtendOutcome::Extended { review: updated })
        } else {
            let req = workflow
                .submit(NewRequest {
                    kind: "review-extension".into(),
                    requester: actor.to_string(),
                    subject: format!("project:{project_id}"),
                    payload: serde_json::json!({"project_id": project_id}),
                    sla_seconds: Some(7 * 24 * 3600),
                })
                .await?;
            Ok(ExtendOutcome::Pending {
                request: Box::new(req),
            })
        }
    }

    pub async fn cost_report(
        &self,
        by: CostDim,
        since: Option<&str>,
        until: Option<&str>,
        scope: Option<&str>,
    ) -> Result<CostReport, RegistryError> {
        cost::report(&self.db, by, since, until, scope).await
    }

    /// Portfolio governance metrics (WS4) over the scoped project set. `scope`
    /// `Some(email)` restricts to projects the caller owns or manages (same rule
    /// as cost/projects visibility); `None` is org-wide (admin/finance, MCP).
    pub async fn governance_metrics(
        &self,
        scope: Option<&str>,
    ) -> Result<GovernanceMetrics, RegistryError> {
        let scoped: Vec<Registration> = self
            .list()
            .await?
            .into_iter()
            .filter(|r| scope.is_none_or(|s| r.owner == s || r.manager == s))
            .collect();
        let id_set: std::collections::HashSet<&str> =
            scoped.iter().map(|r| r.project_id.as_str()).collect();
        let samples = self.promotion_samples(&id_set).await?;
        Ok(governance::compute(
            &scoped,
            &samples,
            &self.governance,
            &asgard_storage::now(),
        ))
    }

    /// Fulfilled-promotion cycle-time samples for the in-scope projects, read
    /// from the workflow request log (request `created_at` → `updated_at`).
    async fn promotion_samples(
        &self,
        ids: &std::collections::HashSet<&str>,
    ) -> Result<Vec<PromotionSample>, RegistryError> {
        let rows = sqlx::query(
            &self
                .db
                .q("SELECT subject, created_at, updated_at, payload, state \
             FROM workflow_requests WHERE kind = 'promotion'"),
        )
        .fetch_all(self.db.pool())
        .await?;
        let mut out = Vec::new();
        for row in rows {
            if row.get::<String, _>("state") != "fulfilled" {
                continue;
            }
            let subject: String = row.get("subject");
            let Some(pid) = subject.strip_prefix("project:") else {
                continue;
            };
            if !ids.contains(pid) {
                continue;
            }
            let payload: String = row.get("payload");
            let target = serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| {
                    v.get("target_classification")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let created: String = row.get("created_at");
            let updated: String = row.get("updated_at");
            if let Some(seconds) = asgard_storage::seconds_between(&created, &updated) {
                out.push(PromotionSample {
                    project_id: pid.to_string(),
                    target,
                    seconds,
                });
            }
        }
        Ok(out)
    }

    pub async fn guidance_list(
        &self,
        include_pending: bool,
        category: Option<&str>,
        q: Option<&str>,
    ) -> Result<Vec<Guidance>, RegistryError> {
        guidance::list(&self.db, include_pending, category, q).await
    }

    pub async fn guidance_get(&self, slug: &str) -> Result<Option<Guidance>, RegistryError> {
        guidance::get(&self.db, slug).await
    }

    /// Submit guidance. `published` is the moderation gate: a normal submission is
    /// `false` (a draft awaiting admin approval); the API only sets it true for an
    /// admin or the boot-time seed. Every write records a version.
    #[allow(clippy::too_many_arguments)]
    pub async fn guidance_put(
        &self,
        slug: Option<&str>,
        title: &str,
        summary: &str,
        body: &str,
        tags: &[String],
        author: &str,
        published: bool,
        category: &str,
    ) -> Result<Guidance, RegistryError> {
        let existed = guidance::get(&self.db, &guidance::slugify(slug.unwrap_or(title)))
            .await?
            .is_some();
        let g = guidance::put(
            &self.db, slug, title, summary, body, tags, author, published, category,
        )
        .await?;
        let action = if existed { "updated" } else { "created" };
        self.record_version("guidance", &g.slug, action, author, &g)
            .await?;
        Ok(g)
    }

    pub async fn guidance_approve(&self, slug: &str) -> Result<(), RegistryError> {
        guidance::set_status(&self.db, slug, "published").await?;
        if let Some(g) = guidance::get(&self.db, slug).await? {
            self.record_version("guidance", slug, "approved", "admin", &g)
                .await?;
        }
        Ok(())
    }

    pub async fn recipe_list(
        &self,
        include_pending: bool,
        q: Option<&str>,
    ) -> Result<Vec<Recipe>, RegistryError> {
        recipes::list(&self.db, include_pending, q).await
    }

    pub async fn recipe_get(&self, slug: &str) -> Result<Option<Recipe>, RegistryError> {
        recipes::get(&self.db, slug).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn recipe_put(
        &self,
        slug: Option<&str>,
        name: &str,
        summary: &str,
        body: &str,
        spec: &serde_json::Value,
        tags: &[String],
        author: &str,
        published: bool,
    ) -> Result<Recipe, RegistryError> {
        let existed = recipes::get(&self.db, &guidance::slugify(slug.unwrap_or(name)))
            .await?
            .is_some();
        let r = recipes::put(
            &self.db, slug, name, summary, body, spec, tags, author, published,
        )
        .await?;
        let action = if existed { "updated" } else { "created" };
        self.record_version("recipes", &r.slug, action, author, &r)
            .await?;
        Ok(r)
    }

    pub async fn recipe_approve(&self, slug: &str) -> Result<(), RegistryError> {
        recipes::set_status(&self.db, slug, "published").await?;
        if let Some(r) = recipes::get(&self.db, slug).await? {
            self.record_version("recipes", slug, "approved", "admin", &r)
                .await?;
        }
        Ok(())
    }

    /// MCP catalog entries. `state` defaults to `active` (the public catalog);
    /// `status` filters by trust tier without hiding either. See [`mcp_servers`].
    pub async fn mcp_server_list(
        &self,
        q: Option<&str>,
        status: Option<&str>,
        state: Option<&str>,
    ) -> Result<Vec<McpServer>, RegistryError> {
        mcp_servers::list(&self.db, q, status, state).await
    }

    pub async fn mcp_server_get(&self, id: &str) -> Result<Option<McpServer>, RegistryError> {
        mcp_servers::get(&self.db, id).await
    }

    /// Publish a new catalog entry owned by `owner`. `approved` mints it as the
    /// company tier (admins / the boot seed); otherwise `community`. Audited.
    pub async fn mcp_server_create(
        &self,
        owner: &str,
        input: &McpServerInput,
        approved: bool,
    ) -> Result<McpServer, RegistryError> {
        let m = mcp_servers::create(&self.db, owner, input, approved).await?;
        self.record_version("mcp_server", &m.id, "created", owner, &m)
            .await?;
        Ok(m)
    }

    pub async fn mcp_server_update(
        &self,
        id: &str,
        input: &McpServerInput,
        actor: &str,
    ) -> Result<Option<McpServer>, RegistryError> {
        let m = mcp_servers::update(&self.db, id, input).await?;
        if let Some(m) = &m {
            self.record_version("mcp_server", id, "updated", actor, m)
                .await?;
        }
        Ok(m)
    }

    pub async fn mcp_server_approve(&self, id: &str, approver: &str) -> Result<(), RegistryError> {
        mcp_servers::set_status(&self.db, id, "approved", Some(approver)).await?;
        if let Some(m) = mcp_servers::get(&self.db, id).await? {
            self.record_version("mcp_server", id, "approved", approver, &m)
                .await?;
        }
        Ok(())
    }

    pub async fn mcp_server_unapprove(&self, id: &str, actor: &str) -> Result<(), RegistryError> {
        mcp_servers::set_status(&self.db, id, "community", None).await?;
        if let Some(m) = mcp_servers::get(&self.db, id).await? {
            self.record_version("mcp_server", id, "unapproved", actor, &m)
                .await?;
        }
        Ok(())
    }

    /// Move an entry through its lifecycle. `action` is the audited verb
    /// (`disabled`/`enabled`/`archived`/`unarchived`); `state` is the result.
    pub async fn mcp_server_set_state(
        &self,
        id: &str,
        state: &str,
        action: &str,
        actor: &str,
    ) -> Result<(), RegistryError> {
        mcp_servers::set_state(&self.db, id, state).await?;
        if let Some(m) = mcp_servers::get(&self.db, id).await? {
            self.record_version("mcp_server", id, action, actor, &m)
                .await?;
        }
        Ok(())
    }

    pub async fn mcp_server_delete(&self, id: &str, actor: &str) -> Result<(), RegistryError> {
        if let Some(m) = mcp_servers::get(&self.db, id).await? {
            self.record_version("mcp_server", id, "deleted", actor, &m)
                .await?;
        }
        mcp_servers::delete(&self.db, id).await
    }

    /// Skills catalog entries. `state` defaults to `active`; `status` filters by trust
    /// tier without hiding either. See [`skills`]. The bundle blob is fetched only via
    /// [`Registry::skill_get_bundle`] for download/export.
    pub async fn skill_list(
        &self,
        q: Option<&str>,
        status: Option<&str>,
        state: Option<&str>,
    ) -> Result<Vec<Skill>, RegistryError> {
        skills::list(&self.db, q, status, state).await
    }

    pub async fn skill_get(&self, id: &str) -> Result<Option<Skill>, RegistryError> {
        skills::get(&self.db, id).await
    }

    pub async fn skill_get_bundle(&self, id: &str) -> Result<Option<String>, RegistryError> {
        skills::get_bundle(&self.db, id).await
    }

    /// Publish a new skill owned by `owner`. `approved` mints it as the company tier
    /// (admins / the boot seed); otherwise `community`. Audited.
    pub async fn skill_create(
        &self,
        owner: &str,
        input: &SkillInput,
        approved: bool,
    ) -> Result<Skill, RegistryError> {
        let s = skills::create(&self.db, owner, input, approved).await?;
        self.record_version("skill", &s.id, "created", owner, &s)
            .await?;
        Ok(s)
    }

    pub async fn skill_update(
        &self,
        id: &str,
        input: &SkillInput,
        actor: &str,
    ) -> Result<Option<Skill>, RegistryError> {
        let s = skills::update(&self.db, id, input).await?;
        if let Some(s) = &s {
            self.record_version("skill", id, "updated", actor, s)
                .await?;
        }
        Ok(s)
    }

    pub async fn skill_approve(&self, id: &str, approver: &str) -> Result<(), RegistryError> {
        skills::set_status(&self.db, id, "approved", Some(approver)).await?;
        if let Some(s) = skills::get(&self.db, id).await? {
            self.record_version("skill", id, "approved", approver, &s)
                .await?;
        }
        Ok(())
    }

    pub async fn skill_unapprove(&self, id: &str, actor: &str) -> Result<(), RegistryError> {
        skills::set_status(&self.db, id, "community", None).await?;
        if let Some(s) = skills::get(&self.db, id).await? {
            self.record_version("skill", id, "unapproved", actor, &s)
                .await?;
        }
        Ok(())
    }

    /// Move a skill through its lifecycle. `action` is the audited verb
    /// (`disabled`/`enabled`/`archived`/`unarchived`); `state` is the result.
    pub async fn skill_set_state(
        &self,
        id: &str,
        state: &str,
        action: &str,
        actor: &str,
    ) -> Result<(), RegistryError> {
        skills::set_state(&self.db, id, state).await?;
        if let Some(s) = skills::get(&self.db, id).await? {
            self.record_version("skill", id, action, actor, &s).await?;
        }
        Ok(())
    }

    /// Persist the latest code-review assist verdict (advisory; escalate-only).
    pub async fn skill_set_review(
        &self,
        id: &str,
        verdict: &serde_json::Value,
    ) -> Result<(), RegistryError> {
        skills::set_review(&self.db, id, verdict).await
    }

    pub async fn skill_delete(&self, id: &str, actor: &str) -> Result<(), RegistryError> {
        if let Some(s) = skills::get(&self.db, id).await? {
            self.record_version("skill", id, "deleted", actor, &s)
                .await?;
        }
        skills::delete(&self.db, id).await
    }

    pub async fn standard_list(
        &self,
        q: Option<&str>,
    ) -> Result<Vec<standards::Standard>, RegistryError> {
        standards::list(&self.db, q).await
    }

    pub async fn standard_get(
        &self,
        id: &str,
    ) -> Result<Option<standards::Standard>, RegistryError> {
        standards::get(&self.db, id).await
    }

    /// Create or update a standard (admin-only at the API layer; always published).
    /// Every write records a version.
    pub async fn standard_put(
        &self,
        id: &str,
        title: &str,
        summary: &str,
        body: &str,
        author: &str,
    ) -> Result<standards::Standard, RegistryError> {
        let existed = standards::get(&self.db, id).await?.is_some();
        let s = standards::put(&self.db, id, title, summary, body, author).await?;
        let action = if existed { "updated" } else { "created" };
        self.record_version("standards", &s.id, action, author, &s)
            .await?;
        Ok(s)
    }

    /// A knowledge doc's version history (newest first). `doc_type` is one of
    /// `guidance` | `recipes` | `standards` | `mcp_server` | `skill`.
    pub async fn knowledge_history(
        &self,
        doc_type: &str,
        slug: &str,
    ) -> Result<Vec<versions::Version>, RegistryError> {
        versions::history(&self.db, doc_type, slug).await
    }

    async fn record_version<T: serde::Serialize>(
        &self,
        doc_type: &str,
        slug: &str,
        action: &str,
        author: &str,
        doc: &T,
    ) -> Result<(), RegistryError> {
        let snapshot = serde_json::to_value(doc).unwrap_or(serde_json::Value::Null);
        versions::append(&self.db, doc_type, slug, action, author, &snapshot).await?;
        Ok(())
    }

    /// Seed the starter guidance + recipes + standards shipped with the binary, but
    /// only into an empty store — never overwrite what a human or agent authored.
    pub async fn seed_knowledge(&self) -> Result<(), RegistryError> {
        if guidance::count(&self.db).await? == 0 {
            for (title, summary, tags, body) in knowledge_seed::GUIDANCE {
                let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
                self.guidance_put(None, title, summary, body, &tags, "asgard", true, "guide")
                    .await?;
            }
        }
        if recipes::count(&self.db).await? == 0 {
            for (name, summary, tags, body, spec_json) in knowledge_seed::RECIPES {
                let spec: serde_json::Value = serde_json::from_str(spec_json)
                    .map_err(|e| RegistryError::Validation(format!("seed recipe '{name}': {e}")))?;
                let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
                self.recipe_put(None, name, summary, body, &spec, &tags, "asgard", true)
                    .await?;
            }
        }
        if standards::count(&self.db).await? == 0 {
            for s in asgard_catalog::standards::STANDARDS {
                self.standard_put(s.id, s.title, s.summary, s.body, "asgard")
                    .await?;
            }
        }
        if mcp_servers::count(&self.db).await? == 0 {
            for (name, summary, install_json, tags, readme) in knowledge_seed::MCP_SERVERS {
                let install: serde_json::Value = serde_json::from_str(install_json)
                    .map_err(|e| RegistryError::Validation(format!("seed mcp '{name}': {e}")))?;
                let input = McpServerInput {
                    name: (*name).to_string(),
                    summary: (*summary).to_string(),
                    readme: (*readme).to_string(),
                    install,
                    tags: tags.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                };
                self.mcp_server_create("asgard", &input, true).await?;
            }
        }
        if skills::count(&self.db).await? == 0 {
            for (name, summary, runtime, tags, files) in knowledge_seed::SKILLS {
                let bundle = asgard_skills::SkillBundle {
                    files: files
                        .iter()
                        .map(|(p, t)| asgard_skills::SkillFile::from_text(*p, t))
                        .collect(),
                };
                let input = SkillInput {
                    name: (*name).to_string(),
                    summary: (*summary).to_string(),
                    runtime: (*runtime).to_string(),
                    tags: tags.iter().map(|s| s.to_string()).collect(),
                    bundle,
                    ..Default::default()
                };
                self.skill_create("asgard", &input, true).await?;
            }
        }
        Ok(())
    }

    /// Mint `proj-YYYY-NNNN` from an atomic per-year counter (max+1, never reused
    /// even under concurrent registration — the predecessor's lowest-free-slot
    /// scheme was racy).
    async fn mint_project_id(&self) -> Result<String, RegistryError> {
        let year = &asgard_storage::now()[..4];
        let scope = format!("project-{year}");
        let value: i64 = sqlx::query_scalar(
            &self
                .db
                .q("INSERT INTO id_counters (scope, value) VALUES (?, 1) \
             ON CONFLICT(scope) DO UPDATE SET value = id_counters.value + 1 RETURNING value"),
        )
        .bind(&scope)
        .fetch_one(self.db.pool())
        .await?;
        Ok(format!("proj-{year}-{value:04}"))
    }

    async fn runtime_meta(
        &self,
        project_id: &str,
    ) -> Result<(String, String, String), RegistryError> {
        let row = sqlx::query(&self.db.q(
            "SELECT display_name, description, created_at FROM projects_runtime WHERE project_id = ?",
        ))
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row
            .map(|r| {
                (
                    r.get::<String, _>("display_name"),
                    r.get::<String, _>("description"),
                    r.get::<String, _>("created_at"),
                )
            })
            .unwrap_or_default())
    }

    #[allow(clippy::too_many_arguments)]
    async fn project_entity(
        &self,
        project_id: &str,
        name: &str,
        description: &str,
        owner: &str,
        manager: &str,
        group: &str,
        cost_center: &str,
        classification: &str,
        budget_usd: f64,
        lifecycle: &str,
    ) -> Result<(), RegistryError> {
        let manifest = Manifest {
            api_version: Some(asgard_catalog::API_VERSION.into()),
            kind: "Project".into(),
            metadata: Metadata {
                name: project_id.to_string(),
                namespace: "default".into(),
                title: Some(name.to_string()),
                description: (!description.is_empty()).then(|| description.to_string()),
                ..Default::default()
            },
            spec: serde_json::json!({
                "id": project_id,
                "owner": owner,
                "manager": manager,
                "group": group,
                "costCenter": cost_center,
                "classification": classification,
                "budgetUsd": budget_usd,
                "lifecycle": lifecycle,
            }),
            relations: vec![],
        };
        self.catalog
            .upsert(&Entity::from_manifest(manifest, Origin::default()))
            .await?;
        Ok(())
    }
}

const PLACEHOLDERS: &[&str] = &["change_me", "replace_me", "your project name", "todo"];

fn is_placeholder(s: &str) -> bool {
    let lc = s.to_lowercase();
    PLACEHOLDERS
        .iter()
        .any(|p| lc == *p || lc.contains("change_me"))
}

/// Self-entered emails are unverified; this is a format check only, not identity
/// validation. Returns the trimmed, lowercased address.
fn normalize_email(raw: &str) -> Result<String, RegistryError> {
    let e = raw.trim().to_lowercase();
    let bad = || RegistryError::Validation(format!("'{raw}' is not a well-formed email address"));
    let (local, domain) = e.split_once('@').ok_or_else(bad)?;
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return Err(bad());
    }
    if domain.split('.').any(|part| part.is_empty()) {
        return Err(bad());
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn registry() -> ProjectRegistry {
        let path =
            std::env::temp_dir().join(format!("asgard-reg-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let allow = GroupAllowlist::new(vec![
            GroupEntry {
                key: "platform".into(),
                display_name: "Platform".into(),
                cost_center: "CC-100".into(),
                active: true,
            },
            GroupEntry {
                key: "research".into(),
                display_name: "Research".into(),
                cost_center: "CC-200".into(),
                active: true,
            },
        ]);
        ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db),
            allow,
            RegistrationPolicy::default(),
        )
    }

    fn input() -> RegisterInput {
        RegisterInput {
            name: "Fraud Detection".into(),
            owner_email: "alice@corp.example".into(),
            manager_email: "bob@corp.example".into(),
            group: "platform".into(),
            classification: None,
            data_class: None,
            budget_usd: Some(50.0),
            description: Some("Detect fraud".into()),
            evidence: Evidence::default(),
        }
    }

    #[tokio::test]
    async fn update_project_renames_without_changing_id_or_evidence() {
        let (reg, wf) = registry_and_workflow().await;
        let mut i = light_ready_input(); // carries real evidence
        i.classification = Some("light-operational".into());
        let r = reg.register(i, "u").await.unwrap();
        let pid = r.project_id.clone();

        let (updated, _) = reg
            .update_project(
                &wf,
                &pid,
                ProjectUpdate {
                    name: Some("Production Aurora".into()),
                    ..Default::default()
                },
                Some(2500.0),
                "user:lead",
            )
            .await
            .unwrap();
        assert_eq!(updated.project_id, pid, "id is the stable handle");
        assert_eq!(updated.name, "Production Aurora");
        // Evidence untouched by a name-only change.
        assert_eq!(updated.evidence.business_owner, "biz@corp.example");
    }

    #[tokio::test]
    async fn budget_within_ceiling_applies_above_routes_to_review() {
        let (reg, wf) = registry_and_workflow().await;
        let r = reg.register(input(), "u").await.unwrap();
        let pid = r.project_id.clone();

        // Within the ceiling → applied immediately.
        let (updated, outcome) = reg
            .update_project(
                &wf,
                &pid,
                ProjectUpdate {
                    budget_usd: Some(400.0),
                    ..Default::default()
                },
                Some(500.0),
                "user:lead",
            )
            .await
            .unwrap();
        assert!(matches!(outcome, BudgetOutcome::Applied));
        assert_eq!(updated.budget_usd, 400.0);

        // Above the ceiling → parked for review, budget unchanged until fulfilled.
        let (parked, outcome) = reg
            .update_project(
                &wf,
                &pid,
                ProjectUpdate {
                    budget_usd: Some(900.0),
                    ..Default::default()
                },
                Some(500.0),
                "user:lead",
            )
            .await
            .unwrap();
        assert_eq!(parked.budget_usd, 400.0, "unchanged until approved");
        let req = match outcome {
            BudgetOutcome::PendingReview(req) => *req,
            other => panic!("expected review, got {other:?}"),
        };

        wf.approve(&req.id, "user:admin", Some("ok")).await.unwrap();
        reg.fulfill_budget(&wf, &req.id, "user:admin")
            .await
            .unwrap();
        let after = reg.get(&pid).await.unwrap().unwrap();
        assert_eq!(after.budget_usd, 900.0, "applied on fulfill");
    }

    #[tokio::test]
    async fn register_mints_id_and_sets_defaults() {
        let r = registry().await;
        let reg = r.register(input(), "user:default/alice").await.unwrap();
        assert!(reg.project_id.starts_with("proj-"));
        assert_eq!(reg.project_id.len(), "proj-2026-0001".len());
        assert_eq!(reg.classification, "poc");
        assert_eq!(reg.data_class, "internal");
        assert_eq!(reg.cost_center, "CC-100");
        assert_eq!(reg.group, "platform");
        assert!(reg.registered);
        assert_eq!(reg.lifecycle, "active");
    }

    #[tokio::test]
    async fn ids_are_monotonic_per_year() {
        let r = registry().await;
        let a = r.register(input(), "u").await.unwrap();
        let b = r.register(input(), "u").await.unwrap();
        assert_ne!(a.project_id, b.project_id);
        assert!(b.project_id.ends_with("0002"));
    }

    #[tokio::test]
    async fn owner_may_self_manage() {
        let r = registry().await;
        let mut i = input();
        i.manager_email = i.owner_email.clone();
        let reg = r.register(i, "u").await.unwrap();
        assert_eq!(reg.owner, reg.manager);
    }

    #[tokio::test]
    async fn manager_omitted_defaults_to_owner_when_optional() {
        let path =
            std::env::temp_dir().join(format!("asgard-reg-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let allow = GroupAllowlist::new(vec![GroupEntry {
            key: "platform".into(),
            display_name: "Platform".into(),
            cost_center: "CC-100".into(),
            active: true,
        }]);
        let r = ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db),
            allow,
            RegistrationPolicy {
                require_manager: false,
                require_group: true,
            },
        );
        let mut i = input();
        i.manager_email = String::new();
        let reg = r.register(i, "u").await.unwrap();
        assert_eq!(reg.manager, reg.owner);
    }

    #[tokio::test]
    async fn manager_required_by_default() {
        let r = registry().await;
        let mut i = input();
        i.manager_email = String::new();
        assert!(matches!(
            r.register(i, "u").await,
            Err(RegistryError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn group_optional_stores_ungrouped() {
        let path =
            std::env::temp_dir().join(format!("asgard-reg-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let r = ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db),
            GroupAllowlist::default(),
            RegistrationPolicy {
                require_manager: false,
                require_group: false,
            },
        );
        let mut i = input();
        i.group = String::new();
        let reg = r.register(i, "u").await.unwrap();
        assert_eq!(reg.group, "");
        assert_eq!(reg.cost_center, "");
    }

    #[tokio::test]
    async fn is_authority_follows_owner_manager() {
        let r = registry().await;
        let reg = r.register(input(), "u").await.unwrap();
        assert!(r
            .is_authority(&reg.project_id, "alice@corp.example", false)
            .await
            .unwrap());
        assert!(r
            .is_authority(&reg.project_id, "bob@corp.example", false)
            .await
            .unwrap());
        assert!(!r
            .is_authority(&reg.project_id, "stranger@corp.example", false)
            .await
            .unwrap());
        // see-all (admin/finance) passes unconditionally, even for unknown ids.
        assert!(r
            .is_authority("proj-2026-9999", "x@corp.example", true)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn group_must_be_in_allowlist() {
        let r = registry().await;
        let mut i = input();
        i.group = "not-a-real-group".into();
        assert!(matches!(
            r.register(i, "u").await,
            Err(RegistryError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn require_active_gates_unregistered_and_decommissioned() {
        let r = registry().await;
        assert!(matches!(
            r.require_active("proj-2026-9999").await,
            Err(RegistryError::NotRegistered(_))
        ));
        let reg = r.register(input(), "u").await.unwrap();
        assert!(r.require_active(&reg.project_id).await.is_ok());
        r.decommission(&reg.project_id, "u", "winding down the project")
            .await
            .unwrap();
        assert!(matches!(
            r.require_active(&reg.project_id).await,
            Err(RegistryError::Inactive(_))
        ));
    }

    #[tokio::test]
    async fn open_allowlist_accepts_any_group() {
        let path =
            std::env::temp_dir().join(format!("asgard-reg-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let r = ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db),
            GroupAllowlist::default(),
            RegistrationPolicy::default(),
        );
        let mut i = input();
        i.group = "anything-goes".into();
        let reg = r.register(i, "u").await.unwrap();
        assert_eq!(reg.group, "anything-goes");
        assert_eq!(reg.cost_center, "anything-goes");
    }

    #[tokio::test]
    async fn evidence_round_trips_on_register() {
        let r = registry().await;
        let mut i = input();
        i.evidence.support_contact = "oncall@corp.example".into();
        i.evidence.security_review_status = "approved".into();
        i.evidence.maintainers = vec!["alice@corp.example".into(), "bob@corp.example".into()];
        i.evidence.primary_data_flows = vec!["s3 -> warehouse".into()];
        let reg = r.register(i, "u").await.unwrap();
        let got = r.get(&reg.project_id).await.unwrap().unwrap();
        assert_eq!(got.evidence.support_contact, "oncall@corp.example");
        assert_eq!(got.evidence.security_review_status, "approved");
        assert_eq!(got.evidence.maintainers.len(), 2);
        assert_eq!(got.evidence.primary_data_flows, vec!["s3 -> warehouse"]);
        // Untouched evidence fields stay empty, never NULL.
        assert_eq!(got.evidence.runbook_url, "");
        assert!(got.evidence.critical_dependencies.is_empty());
    }

    #[tokio::test]
    async fn update_evidence_replaces_and_requires_existing() {
        let r = registry().await;
        let reg = r.register(input(), "u").await.unwrap();
        let mut ev = Evidence {
            runbook_url: "https://runbook".into(),
            critical_dependencies: vec!["postgres".into()],
            ..Default::default()
        };
        let updated = r
            .update_evidence(&reg.project_id, ev.clone(), "u")
            .await
            .unwrap();
        assert_eq!(updated.evidence.runbook_url, "https://runbook");
        assert_eq!(updated.evidence.critical_dependencies, vec!["postgres"]);
        // PUT semantics: a subsequent update with the field cleared wipes it.
        ev.runbook_url = String::new();
        let cleared = r.update_evidence(&reg.project_id, ev, "u").await.unwrap();
        assert_eq!(cleared.evidence.runbook_url, "");
        // Unknown project rejected.
        assert!(matches!(
            r.update_evidence("proj-2026-9999", Evidence::default(), "u")
                .await,
            Err(RegistryError::NotRegistered(_))
        ));
    }

    #[tokio::test]
    async fn evidence_enum_is_validated() {
        let r = registry().await;
        let mut i = input();
        i.evidence.security_review_status = "totally-bogus".into();
        assert!(matches!(
            r.register(i, "u").await,
            Err(RegistryError::Validation(_))
        ));
        let reg = r.register(input(), "u").await.unwrap();
        let bad = Evidence {
            state_loss_posture: "nope".into(),
            ..Default::default()
        };
        assert!(matches!(
            r.update_evidence(&reg.project_id, bad, "u").await,
            Err(RegistryError::Validation(_))
        ));
    }

    #[test]
    fn email_format_is_checked() {
        assert!(normalize_email("a@b.com").is_ok());
        assert!(normalize_email("no-at-sign").is_err());
        assert!(normalize_email("a@nodot").is_err());
        assert!(normalize_email("@b.com").is_err());
    }

    // --- WS2 promotion orchestration -------------------------------------

    async fn registry_and_workflow() -> (ProjectRegistry, WorkflowEngine) {
        let path =
            std::env::temp_dir().join(format!("asgard-prom-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let allow = GroupAllowlist::new(vec![GroupEntry {
            key: "platform".into(),
            display_name: "Platform".into(),
            cost_center: "CC-100".into(),
            active: true,
        }]);
        let reg = ProjectRegistry::new(
            db.clone(),
            GatewayRepo::new(db.clone()),
            CatalogRepo::new(db.clone()),
            allow,
            RegistrationPolicy::default(),
        );
        let wf = WorkflowEngine::new(
            db,
            std::sync::Arc::new(asgard_policy::CedarEngine::new().unwrap()),
        );
        (reg, wf)
    }

    fn light_ready_input() -> RegisterInput {
        let mut i = input();
        let ev = &mut i.evidence;
        ev.repo_or_source_url = "https://git".into();
        ev.business_owner = "biz@corp.example".into();
        ev.technical_owner = "tech@corp.example".into();
        ev.team_or_org_of_record = "Platform".into();
        ev.support_contact = "oncall@corp.example".into();
        ev.runbook_url = "https://runbook".into();
        ev.monitoring_or_logs_url = "https://logs".into();
        ev.ci_status_url = "N/A".into();
        ev.primary_data_flows = vec!["s3 -> warehouse".into()];
        ev.critical_flow_test_or_eval_url = "https://eval".into();
        ev.state_loss_posture = "stateless".into();
        ev.requested_classification = "light-operational".into();
        i
    }

    fn wide_ready_input() -> RegisterInput {
        let mut i = light_ready_input();
        let ev = &mut i.evidence;
        ev.security_review_status = "approved".into();
        ev.architecture_summary_url = "https://arch".into();
        ev.critical_dependencies = vec!["postgres".into()];
        ev.incident_path = "pagerduty".into();
        ev.slo_or_service_target = "99.9".into();
        ev.rpo_rto = "1h/4h".into();
        ev.decommission_path = "documented".into();
        i
    }

    #[tokio::test]
    async fn promotion_to_light_auto_approves_and_fulfills() {
        let (r, wf) = registry_and_workflow().await;
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let req = r
            .request_promotion(
                &wf,
                &reg.project_id,
                "light-operational",
                "user:default/alice",
            )
            .await
            .unwrap();
        assert_eq!(
            req.state,
            State::Approved,
            "clean Light should auto-approve"
        );
        let done = r
            .fulfill_promotion(&wf, &req.id, "user:default/alice")
            .await
            .unwrap();
        assert_eq!(done.state, State::Fulfilled);
        let after = r.get(&reg.project_id).await.unwrap().unwrap();
        assert_eq!(after.classification, "light-operational");
        // The met request is cleared from the evidence record.
        assert_eq!(after.evidence.requested_classification, "");
    }

    #[tokio::test]
    async fn promotion_with_missing_evidence_returns_to_submitter() {
        let (r, wf) = registry_and_workflow().await;
        // Default input has no evidence → Light requirements unmet (self-fixable).
        let reg = r.register(input(), "u").await.unwrap();
        let req = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(
            req.state,
            State::Flagged,
            "missing evidence is self-fixable → return to submitter, not a human"
        );
        // The approver Cedar assigned is preserved for a later escalation.
        assert_eq!(req.approver.as_deref(), Some("group:default/platform"));
        // Not fulfillable while flagged.
        assert!(r.fulfill_promotion(&wf, &req.id, "u").await.is_err());
        // Submitter escalates → now a human owns it.
        let esc = wf.escalate(&req.id, "u").await.unwrap();
        assert_eq!(esc.state, State::Requested);
    }

    #[tokio::test]
    async fn clean_wide_routes_to_human_no_findings() {
        let (r, wf) = registry_and_workflow().await;
        let reg = r.register(wide_ready_input(), "u").await.unwrap();
        // Move to light first (auto). Fulfilling clears requested_classification.
        let req = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        r.fulfill_promotion(&wf, &req.id, "u").await.unwrap();
        // Re-declare the next target so Wide evidence is complete.
        let cur = r.get(&reg.project_id).await.unwrap().unwrap();
        let mut ev = cur.evidence.clone();
        ev.requested_classification = "wide-operational".into();
        r.update_evidence(&reg.project_id, ev, "u").await.unwrap();
        // Clean Light -> Wide: no findings, but Wide always needs a human by tier.
        let wreq = r
            .request_promotion(&wf, &reg.project_id, "wide-operational", "u")
            .await
            .unwrap();
        assert_eq!(
            wreq.state,
            State::Requested,
            "clean wide has no findings → straight to the human approver, not flagged"
        );
        assert_eq!(wreq.approver.as_deref(), Some("group:default/platform"));
    }

    struct ConcernPanel;
    #[async_trait::async_trait]
    impl ReviewerPanel for ConcernPanel {
        async fn review(&self, _: &Registration, _: &str, _: &EvidenceVerdict) -> ReviewerOutcome {
            ReviewerOutcome {
                passed: false,
                added_exception_signals: vec![
                    "llm-judge: ci_status_url 'N/A' is a placeholder, not evidence".into(),
                ],
                findings: vec!["ci_status_url 'N/A' is a placeholder, not evidence".into()],
                summary: "1 reviewer concern".into(),
                reviewer_ids: vec!["llm-judge".into()],
                verdicts_json: vec![serde_json::json!({
                    "reviewer_id": "llm-judge", "kind": "llm-judge", "disposition": "concern",
                    "model": "model:default/mock", "cost_usd": 0.0,
                })],
            }
        }
    }

    #[tokio::test]
    async fn reviewer_concern_flags_clean_light_and_alerts() {
        let (r, wf) = registry_and_workflow().await;
        let r = r.with_reviewer_panel(std::sync::Arc::new(ConcernPanel));
        // Evidence is complete for Light, so only the reviewer's concern blocks it.
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let req = r
            .request_promotion(
                &wf,
                &reg.project_id,
                "light-operational",
                "user:default/alice",
            )
            .await
            .unwrap();
        assert_eq!(
            req.state,
            State::Flagged,
            "a reviewer concern returns an otherwise-clean Light promotion to the submitter"
        );
        assert_eq!(req.payload["review_passed"], serde_json::json!(false));
        assert!(!req.payload["review_findings"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(req.payload["reviewers_ran"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "llm-judge"));
        // The verdict is persisted for audit.
        let n: i64 = sqlx::query_scalar(
            &r.db
                .q("SELECT COUNT(*) FROM promotion_reviews WHERE request_id = ?"),
        )
        .bind(&req.id)
        .fetch_one(r.db.pool())
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn re_request_supersedes_open_promotion() {
        let (r, wf) = registry_and_workflow().await;
        let r = r.with_reviewer_panel(std::sync::Arc::new(ConcernPanel));
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let first = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(first.state, State::Flagged);
        let second = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        // The stale flagged attempt is superseded; exactly one open promotion remains.
        let first_after = wf.get(&first.id).await.unwrap().unwrap();
        assert_eq!(first_after.state, State::Cancelled);
        let open = r
            .open_promotions(&wf, &format!("project:{}", reg.project_id))
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, second.id);
    }

    /// A panel with an async reviewer: `request_promotion` must defer it to the
    /// worker (park `Reviewing` + enqueue) rather than run it inline.
    struct AsyncPanel {
        concern: bool,
    }
    #[async_trait::async_trait]
    impl ReviewerPanel for AsyncPanel {
        async fn review(&self, _: &Registration, _: &str, _: &EvidenceVerdict) -> ReviewerOutcome {
            if self.concern {
                ReviewerOutcome {
                    passed: false,
                    added_exception_signals: vec!["code-review: violates standards".into()],
                    findings: vec!["unhandled error path".into()],
                    summary: "1 reviewer finding(s)".into(),
                    reviewer_ids: vec!["code-review".into()],
                    verdicts_json: vec![serde_json::json!({
                        "reviewer_id": "code-review", "kind": "code-review",
                        "disposition": "concern", "model": "model:default/mock", "cost_usd": 0.0,
                    })],
                }
            } else {
                ReviewerOutcome {
                    reviewer_ids: vec!["code-review".into()],
                    ..Default::default()
                }
            }
        }
        fn has_async(&self, _target: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn async_reviewer_parks_in_reviewing_and_enqueues() {
        let (r, wf) = registry_and_workflow().await;
        let r = r.with_reviewer_panel(std::sync::Arc::new(AsyncPanel { concern: false }));
        let reg = r.register(light_ready_input(), "u").await.unwrap();

        // Clean Light + an async reviewer → deferred, not auto-approved inline.
        let req = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(req.state, State::Reviewing);
        // The pre-review state (a clean Light would auto-approve) is stashed.
        assert_eq!(
            req.payload["pre_review_state"],
            serde_json::json!("approved")
        );
        // A pending job is queued for the worker.
        let job = r.jobs().latest_for_request(&req.id).await.unwrap().unwrap();
        assert_eq!(job.status, "pending");
        assert_eq!(job.target, "light-operational");

        // Worker, clean verdict → restore the stashed Approved state.
        let outcome = ReviewerOutcome {
            reviewer_ids: vec!["code-review".into()],
            ..Default::default()
        };
        let reviewing = wf.get(&req.id).await.unwrap().unwrap();
        let done = r
            .finalize_promotion(
                &wf,
                reviewing,
                &reg.project_id,
                "light-operational",
                State::Approved,
                &outcome,
                false,
                "system",
            )
            .await
            .unwrap();
        assert_eq!(done.state, State::Approved);
        // Fulfillable now, like any approved promotion.
        r.fulfill_promotion(&wf, &done.id, "system").await.unwrap();
    }

    #[tokio::test]
    async fn async_reviewer_findings_flag_and_persist() {
        let (r, wf) = registry_and_workflow().await;
        let panel = AsyncPanel { concern: true };
        let r = r.with_reviewer_panel(std::sync::Arc::new(AsyncPanel { concern: true }));
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let req = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(req.state, State::Reviewing);

        // Worker runs the panel → a concern → finalize flags it (escalate-only).
        let cur = r.get(&reg.project_id).await.unwrap().unwrap();
        let verdict = promotion::evaluate(&cur, "light-operational", r.requirements());
        let outcome = panel.review(&cur, "light-operational", &verdict).await;
        let has_exc =
            !verdict.exception_signals.is_empty() || !outcome.added_exception_signals.is_empty();
        let reviewing = wf.get(&req.id).await.unwrap().unwrap();
        let flagged = r
            .finalize_promotion(
                &wf,
                reviewing,
                &reg.project_id,
                "light-operational",
                State::Approved,
                &outcome,
                has_exc,
                "system",
            )
            .await
            .unwrap();
        assert_eq!(flagged.state, State::Flagged);
        // The verdict is persisted for audit.
        let n: i64 = sqlx::query_scalar(
            &r.db
                .q("SELECT COUNT(*) FROM promotion_reviews WHERE request_id = ?"),
        )
        .bind(&req.id)
        .fetch_one(r.db.pool())
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn re_request_supersedes_in_flight_review() {
        let (r, wf) = registry_and_workflow().await;
        let r = r.with_reviewer_panel(std::sync::Arc::new(AsyncPanel { concern: false }));
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let first = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(first.state, State::Reviewing);
        // A re-run cancels the in-flight Reviewing attempt.
        let second = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        assert_eq!(
            wf.get(&first.id).await.unwrap().unwrap().state,
            State::Cancelled
        );
        let open = r
            .open_promotions(&wf, &format!("project:{}", reg.project_id))
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, second.id);
    }

    #[tokio::test]
    async fn two_step_jump_is_rejected() {
        let (r, wf) = registry_and_workflow().await;
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        assert!(matches!(
            r.request_promotion(&wf, &reg.project_id, "wide-operational", "u")
                .await,
            Err(RegistryError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn demote_requires_reason_and_downward_target() {
        let (r, wf) = registry_and_workflow().await;
        let reg = r.register(light_ready_input(), "u").await.unwrap();
        let req = r
            .request_promotion(&wf, &reg.project_id, "light-operational", "u")
            .await
            .unwrap();
        r.fulfill_promotion(&wf, &req.id, "u").await.unwrap();
        // Empty reason rejected.
        assert!(matches!(
            r.demote(&reg.project_id, "poc", "u", "  ").await,
            Err(RegistryError::Validation(_))
        ));
        // Upward "demotion" rejected.
        assert!(matches!(
            r.demote(&reg.project_id, "wide-operational", "u", "x")
                .await,
            Err(RegistryError::Validation(_))
        ));
        // Valid downward demotion.
        let after = r
            .demote(&reg.project_id, "poc", "u", "scope reduced to a prototype")
            .await
            .unwrap();
        assert_eq!(after.classification, "poc");
    }

    #[tokio::test]
    async fn checklist_reports_next_tier_and_gaps() {
        let (r, _wf) = registry_and_workflow().await;
        let reg = r.register(input(), "u").await.unwrap();
        let cl = r.promotion_checklist(&reg.project_id).await.unwrap();
        assert_eq!(cl.current, "poc");
        assert_eq!(cl.next_tier.as_deref(), Some("light-operational"));
        let v = cl.verdict.unwrap();
        assert!(!v.evidence_complete);
        assert!(!v.missing.is_empty());
    }

    // --- WS3 review-date engine ------------------------------------------

    async fn set_review_date(r: &ProjectRegistry, pid: &str, date: &str) {
        sqlx::query(
            &r.db
                .q("UPDATE projects_runtime SET review_date = ? WHERE project_id = ?"),
        )
        .bind(date)
        .bind(pid)
        .execute(r.db.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn poc_gets_a_review_deadline_at_registration() {
        let (r, _wf) = registry_and_workflow().await;
        let reg = r.register(input(), "u").await.unwrap();
        assert!(
            !reg.review_date.is_empty(),
            "POC should get a review deadline"
        );
        assert_eq!(reg.review_state, "ok");
    }

    #[tokio::test]
    async fn sweep_flags_overdue_review_once_and_blocks_nothing() {
        let (r, _wf) = registry_and_workflow().await;
        let reg = r.register(input(), "u").await.unwrap();
        // Time-warp the deadline into the past.
        set_review_date(&r, &reg.project_id, "2000-01-01T00:00:00.000Z").await;
        let s1 = r.sweep("system").await.unwrap();
        assert!(s1.newly_expired.contains(&reg.project_id));
        let after = r.get(&reg.project_id).await.unwrap().unwrap();
        assert_eq!(after.review_state, "expired");
        // Expiry is a flag — lifecycle is untouched and the gate still passes.
        assert_eq!(after.lifecycle, "active");
        assert!(r.require_active(&reg.project_id).await.is_ok());
        // Idempotent: a second sweep does not re-report or re-audit.
        let s2 = r.sweep("system").await.unwrap();
        assert!(s2.newly_expired.is_empty());
    }

    #[tokio::test]
    async fn first_extend_is_automatic_second_routes_to_human() {
        let (r, wf) = registry_and_workflow().await;
        let reg = r.register(input(), "u").await.unwrap();
        set_review_date(&r, &reg.project_id, "2000-01-01T00:00:00.000Z").await;
        r.sweep("system").await.unwrap();
        // First extension is automatic and clears the flag.
        match r.extend_review(&wf, &reg.project_id, "u").await.unwrap() {
            ExtendOutcome::Extended { review } => {
                assert_eq!(review.review_extensions, 1);
                assert_eq!(review.review_state, "ok");
            }
            _ => panic!("first extend should be automatic"),
        }
        // The allowance (default 1) is now spent → a human-approval request.
        match r.extend_review(&wf, &reg.project_id, "u").await.unwrap() {
            ExtendOutcome::Pending { request } => {
                assert_eq!(request.kind, "review-extension");
            }
            _ => panic!("second extend should route to a human"),
        }
    }

    #[tokio::test]
    async fn sweep_surfaces_stack_exception_without_renewal() {
        let (r, _wf) = registry_and_workflow().await;
        let mut i = input();
        i.evidence.stack_exception = "uses an unsupported runtime".into();
        let reg = r.register(i, "u").await.unwrap();
        let s = r.sweep("system").await.unwrap();
        assert!(s.expired_exceptions.contains(&reg.project_id));
    }
}
