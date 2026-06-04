//! MCP server (brief §4 surface 2): the agent-era equivalent of the Backstage
//! UI. Built on the official Rust SDK (`rmcp`), exposed over two transports:
//! Streamable HTTP (mounted at `/mcp` in `serve()`) and stdio (`asgard mcp`).
//! One [`AsgardMcp`] `ServerHandler` backs both; tool bodies call the same domain
//! services the REST surface does — no business logic lives here.
//!
//! ## Authentication & scoping
//! Over HTTP, [`http_router`] gates `/mcp` with an auth middleware that resolves
//! the `Authorization: Bearer <token>` into an [`McpAuth`] principal: a user PAT
//! (`asg_pat_…`) → a `User` (acts across every project they own/manage; can
//! register), anything else → a `Project` via [`GatewayRepo::verify_key`]. The
//! Streamable-HTTP transport hands the `http::request::Parts` (carrying that
//! extension) to each tool, so project-scoped tools (`request_resource`,
//! `get_secret`, …) authorize the target project per [`AsgardMcp::resolve_project`]
//! — a project id is never a spoofable argument. Over stdio there is no token; the
//! project comes from `ASGARD_PROJECT` (local trust), passed as `default_project`.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use asgard_catalog::{seed, CatalogRepo, ListFilter};
use asgard_gateway::{Gateway, GatewayRepo};
use asgard_identity::{IdentityService, Role, PAT_PREFIX};
use asgard_provision::{ProvisionService, RollupDim};
use asgard_registry::{CostDim, ProjectRegistry, RegisterInput};
use asgard_workflow::WorkflowEngine;

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, GetPromptRequestParams, GetPromptResult, Implementation,
    ListPromptsResult, PaginatedRequestParams, PromptMessage, PromptMessageRole, ProtocolVersion,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router, ErrorData as McpError,
    RoleServer, ServerHandler,
};

/// Per-request authentication principal injected by the `/mcp` auth middleware.
/// A project virtual key authenticates as exactly one project (today's path); a
/// user PAT authenticates as a person who can act on every project they own or
/// manage (and register new ones). The control-plane tools resolve the target
/// project differently per variant — see [`AsgardMcp::resolve_project`].
#[derive(Debug, Clone)]
pub enum McpAuth {
    Project { project_id: String },
    User { email: String, role: String },
}

#[derive(Clone)]
pub struct AsgardMcp {
    catalog: CatalogRepo,
    gateway: Arc<Gateway>,
    registry: ProjectRegistry,
    workflow: Arc<WorkflowEngine>,
    provision: ProvisionService,
    /// stdio/local fallback project (from `ASGARD_PROJECT`); `None` over HTTP,
    /// where the project comes from the authenticated key instead.
    default_project: Option<String>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

// --- typed tool inputs (first-class JSON Schemas for agents) -----------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CatalogSearchArgs {
    /// Filter by entity kind (e.g. Agent, Component).
    pub kind: Option<String>,
    /// Free-text query over titles/names.
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CatalogGetArgs {
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectArg {
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectIdRequired {
    pub project_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterProjectArgs {
    pub name: String,
    /// The project owner. On a user-token connection this is ignored and stamped
    /// from the authenticated user.
    #[serde(default)]
    pub owner_email: String,
    /// Optional per operator policy; blank defaults to the owner (self-manage).
    #[serde(default)]
    pub manager_email: String,
    /// Optional per operator policy; blank stores an ungrouped project.
    #[serde(default)]
    pub group: String,
    pub classification: Option<String>,
    pub data_class: Option<String>,
    pub budget_usd: Option<f64>,
    pub description: Option<String>,
    pub requester: Option<String>,
    #[serde(flatten)]
    pub evidence: asgard_registry::Evidence,
}

/// Project id plus the full evidence record (PUT semantics — the submitted block
/// replaces the stored one).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateProjectArgs {
    pub project_id: Option<String>,
    #[serde(flatten)]
    pub evidence: asgard_registry::Evidence,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdArg {
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CostReportArgs {
    pub by: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuidanceGetArgs {
    /// The guidance slug (see guidance_list).
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuidancePutArgs {
    /// Optional explicit slug; derived from the title when omitted. Reusing a slug
    /// updates that doc.
    #[serde(default)]
    pub slug: Option<String>,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Category facet: "best-practice", "guide" (default), or "reference".
    #[serde(default)]
    pub category: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecipeGetArgs {
    /// The recipe slug (see recipe_list).
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecipePutArgs {
    #[serde(default)]
    pub slug: Option<String>,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    /// The narrated runbook (markdown) — the primary content. Write it richly.
    #[serde(default)]
    pub body: String,
    /// Optional machine-readable at-a-glance: { description, inputs, steps, outputs }.
    #[serde(default)]
    pub spec: serde_json::Value,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectCostArgs {
    pub project_id: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CostSeriesArgs {
    pub project: String,
    pub from: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CostByArgs {
    pub by: Option<String>,
    pub from: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CostAnomaliesArgs {
    pub project: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AsOfArgs {
    pub as_of: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CostMoversArgs {
    pub as_of: Option<String>,
    pub top: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestResourceArgs {
    pub project_id: Option<String>,
    pub resource_type: String,
    pub name: String,
    pub spec: Option<serde_json::Value>,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PromotionArgs {
    /// Target project (required on a user token; omit on a project key).
    pub project_id: Option<String>,
    /// The tier to promote to — must be exactly one step above the current tier
    /// (poc → light-operational → wide-operational → critical-path).
    pub target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeprovisionArgs {
    pub resource_id: String,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SecretArgs {
    /// Target project. Omit on a project-key connection (the key's project is
    /// used); required on a user-PAT connection (which project's secret).
    pub project_id: Option<String>,
    pub name: String,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeedPlanArgs {
    /// Languages the repo is written in (e.g. ["rust", "typescript"]).
    pub languages: Option<Vec<String>>,
    /// Free-text description of the work (e.g. "build a React dashboard").
    pub task: Option<String>,
    /// Rigor tier: "minimal", "standard" (default), or "strict".
    pub tier: Option<String>,
}

// --- domain layer (no MCP types; directly unit-testable) ---------------------

const DEFAULT_REQUESTER: &str = "agent:default/unknown";

impl AsgardMcp {
    pub fn new(
        catalog: CatalogRepo,
        gateway: Arc<Gateway>,
        registry: ProjectRegistry,
        workflow: Arc<WorkflowEngine>,
        provision: ProvisionService,
        default_project: Option<String>,
    ) -> Self {
        AsgardMcp {
            catalog,
            gateway,
            registry,
            workflow,
            provision,
            default_project,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    fn auth(ctx: &RequestContext<RoleServer>) -> Option<McpAuth> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<McpAuth>().cloned())
    }

    /// The project a scoped tool acts on, authorized by the request principal:
    /// - **Project key** — locked to that key's project (a differing `project_id`
    ///   argument is denied).
    /// - **User PAT** — `project_id` is required and authorized via the shared
    ///   ownership predicate (owner/manager, or admin/finance see-all).
    /// - **None (stdio)** — falls back to the argument or `default_project`.
    async fn resolve_project(
        &self,
        ctx: &RequestContext<RoleServer>,
        arg: Option<String>,
    ) -> Result<String, String> {
        let arg = arg.filter(|s| !s.is_empty());
        match Self::auth(ctx) {
            Some(McpAuth::Project { project_id }) => {
                if let Some(p) = arg {
                    if p != project_id {
                        return Err("cross-project access denied".into());
                    }
                }
                Ok(project_id)
            }
            Some(McpAuth::User { email, role }) => {
                let pid =
                    arg.ok_or_else(|| "project_id is required for a user token".to_string())?;
                self.authorize_user(&email, &role, &pid).await.map(|_| pid)
            }
            None => arg
                .or_else(|| self.default_project.clone())
                .ok_or_else(|| "project_id required (no authenticated project)".into()),
        }
    }

    /// Authorize a user principal for a project (owner/manager, or see-all role).
    async fn authorize_user(&self, email: &str, role: &str, pid: &str) -> Result<(), String> {
        let see_all = Role::parse(role).can(asgard_identity::Capability::ViewAllCost);
        match self.registry.is_authority(pid, email, see_all).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "not authorized for project {pid} (you must own or manage it)"
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn do_catalog_search(&self, a: CatalogSearchArgs) -> Result<String, String> {
        let filter = ListFilter {
            kind: a.kind,
            query: a.query,
            ..Default::default()
        };
        let entities = self
            .catalog
            .list(&filter)
            .await
            .map_err(|e| e.to_string())?;
        let summary: Vec<_> = entities
            .iter()
            .map(|e| json!({"ref": e.entity_ref(), "title": e.metadata.title, "lifecycle": e.lifecycle.as_str()}))
            .collect();
        Ok(serde_json::to_string(&summary).unwrap_or_default())
    }

    async fn do_catalog_get(&self, a: CatalogGetArgs) -> Result<String, String> {
        let ns = a.namespace.as_deref().unwrap_or("default");
        match self
            .catalog
            .get(&a.kind, ns, &a.name)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(e) => Ok(serde_json::to_string(&e).unwrap_or_default()),
            None => Err(format!("not found: {}:{ns}/{}", a.kind, a.name)),
        }
    }

    async fn do_list_services(&self) -> Result<String, String> {
        let services: Vec<_> = self
            .provision
            .catalog()
            .list()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(json!({})))
            .collect();
        Ok(serde_json::to_string(&services).unwrap_or_default())
    }

    fn do_get_service(&self, id: &str) -> Result<String, String> {
        match self.provision.catalog().get(id) {
            Some(m) => Ok(serde_json::to_string(m).unwrap_or_default()),
            None => Err(format!(
                "unknown service '{id}'; call list_services for ids"
            )),
        }
    }

    /// `owner_override` stamps the owner from the authenticated user (a PAT
    /// caller can only register projects they own) — the onboarding loop: one
    /// PAT registers a project and is immediately authorized to provision it.
    async fn do_register_project(
        &self,
        a: RegisterProjectArgs,
        owner_override: Option<String>,
    ) -> Result<String, String> {
        let requester = a.requester.clone();
        let owner_email = owner_override.clone().unwrap_or(a.owner_email);
        let input = RegisterInput {
            name: a.name,
            owner_email,
            manager_email: a.manager_email,
            group: a.group,
            classification: a.classification,
            data_class: a.data_class,
            budget_usd: a.budget_usd,
            description: a.description,
            evidence: a.evidence,
        };
        let actor = owner_override
            .as_deref()
            .map(|e| format!("user:{e}"))
            .unwrap_or_else(|| {
                requester
                    .as_deref()
                    .unwrap_or("agent:default/unknown")
                    .to_string()
            });
        let actor = actor.as_str();
        let reg = self
            .registry
            .register(input, actor)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&reg).unwrap_or_default())
    }

    async fn do_update_project(
        &self,
        pid: &str,
        evidence: asgard_registry::Evidence,
        actor: &str,
    ) -> Result<String, String> {
        let reg = self
            .registry
            .update_evidence(pid, evidence, actor)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&reg).unwrap_or_default())
    }

    async fn do_project_get(&self, pid: &str) -> Result<String, String> {
        match self.registry.get(pid).await.map_err(|e| e.to_string())? {
            Some(reg) => Ok(serde_json::to_string(&reg).unwrap_or_default()),
            None => Err(format!("project '{pid}' is not registered")),
        }
    }

    async fn do_project_state(&self, pid: &str) -> Result<String, String> {
        match self
            .gateway
            .repo()
            .get_project(pid)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(rt) => Ok(json!({
                "project_id": rt.project_id,
                "budget_usd": rt.budget_usd,
                "spent_usd": rt.spent_usd,
                "killed": rt.killed,
                "data_class": rt.data_class,
            })
            .to_string()),
            None => Err(format!("unknown project: {pid}")),
        }
    }

    async fn do_gateway_credential(&self, pid: &str) -> Result<String, String> {
        self.registry
            .require_active(pid)
            .await
            .map_err(|e| e.to_string())?;
        let minted = self
            .gateway
            .repo()
            .mint_key(pid, Some("mcp"))
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"key": minted.plaintext, "prefix": minted.prefix, "project_id": pid})
                .to_string(),
        )
    }

    fn do_list_groups(&self) -> Result<String, String> {
        let groups: Vec<_> = self
            .registry
            .allowlist()
            .entries()
            .iter()
            .filter(|e| e.active)
            .map(|e| json!({"key": e.key, "display_name": e.display_name, "cost_center": e.cost_center}))
            .collect();
        Ok(json!({"open": self.registry.allowlist().is_open(), "groups": groups}).to_string())
    }

    async fn do_list_standards(&self) -> Result<String, String> {
        let list = self
            .registry
            .standard_list(None)
            .await
            .map_err(|e| e.to_string())?;
        let out: Vec<_> = list
            .iter()
            .map(|s| json!({"id": s.id, "title": s.title, "summary": s.summary}))
            .collect();
        Ok(serde_json::to_string(&out).unwrap_or_default())
    }

    async fn do_get_standards(&self, id: &str) -> Result<String, String> {
        match self
            .registry
            .standard_get(id)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(s) => Ok(json!({"id": s.id, "title": s.title, "body": s.body}).to_string()),
            None => Err(format!(
                "unknown standard '{id}'; call list_standards for ids"
            )),
        }
    }

    async fn do_guidance_list(&self) -> Result<String, String> {
        // Agents read approved guidance only.
        let list = self
            .registry
            .guidance_list(false, None, None)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    async fn do_guidance_get(&self, slug: &str) -> Result<String, String> {
        match self
            .registry
            .guidance_get(slug)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(g) => serde_json::to_string(&g).map_err(|e| e.to_string()),
            None => Err(format!(
                "unknown guidance '{slug}'; call guidance_list for slugs"
            )),
        }
    }

    async fn do_guidance_put(&self, a: GuidancePutArgs) -> Result<String, String> {
        let author = self
            .default_project
            .clone()
            .unwrap_or_else(|| "agent".into());
        let g = self
            .registry
            .guidance_put(
                a.slug.as_deref(),
                &a.title,
                &a.summary,
                &a.body,
                &a.tags,
                &author,
                false,
                &a.category,
            )
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&g).map_err(|e| e.to_string())
    }

    async fn do_recipe_list(&self) -> Result<String, String> {
        let list = self
            .registry
            .recipe_list(false, None)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    async fn do_recipe_get(&self, slug: &str) -> Result<String, String> {
        match self
            .registry
            .recipe_get(slug)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(r) => serde_json::to_string(&r).map_err(|e| e.to_string()),
            None => Err(format!(
                "unknown recipe '{slug}'; call recipe_list for slugs"
            )),
        }
    }

    async fn do_recipe_put(&self, a: RecipePutArgs) -> Result<String, String> {
        let author = self
            .default_project
            .clone()
            .unwrap_or_else(|| "agent".into());
        let r = self
            .registry
            .recipe_put(
                a.slug.as_deref(),
                &a.name,
                &a.summary,
                &a.body,
                &a.spec,
                &a.tags,
                &author,
                false,
            )
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&r).map_err(|e| e.to_string())
    }

    async fn do_cost_report(&self, a: CostReportArgs) -> Result<String, String> {
        let by = CostDim::parse(a.by.as_deref().unwrap_or("project")).ok_or_else(|| {
            "by must be one of: project, owner, manager, group, classification, model, provider"
                .to_string()
        })?;
        let report = self
            .registry
            .cost_report(by, a.since.as_deref(), a.until.as_deref(), None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&report).unwrap_or_default())
    }

    async fn do_project_cost(&self, pid: &str, a: ProjectCostArgs) -> Result<String, String> {
        let reg = self
            .registry
            .get(pid)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("project not registered: {pid}"))?;
        let window = match (a.start, a.end) {
            (Some(s), Some(e)) => asgard_provision::CostWindow { start: s, end: e },
            _ => asgard_provision::CostWindow::current_month(),
        };
        let infra = self
            .provision
            .project_cost(pid, &window)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "project_id": pid,
            "cost_center": reg.cost_center,
            "model_usd_to_date": reg.spent_usd,
            "infra_estimated_monthly_usd": infra.estimated_monthly_usd,
            "infra_actual": infra.actual,
            "resources": infra.resources,
            "window": { "start": window.start, "end": window.end },
        })
        .to_string())
    }

    async fn do_cost_series(&self, a: CostSeriesArgs) -> Result<String, String> {
        let today = asgard_provision::today();
        let from = a.from.unwrap_or_else(|| month_start(&today));
        let until = a.until.unwrap_or(today);
        let rows = self
            .provision
            .rollup_repo()
            .series(&a.project, &from, &until)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&rows).unwrap_or_default())
    }

    async fn do_cost_by(&self, a: CostByArgs) -> Result<String, String> {
        let dim = RollupDim::parse(a.by.as_deref().unwrap_or("project")).ok_or_else(|| {
            "by must be one of: project, owner, manager, group, cost_center, classification, service".to_string()
        })?;
        let today = asgard_provision::today();
        let from = a.from.unwrap_or_else(|| month_start(&today));
        let until = a.until.unwrap_or(today);
        let rows = self
            .provision
            .rollup_repo()
            .by_dimension(dim, &from, &until)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({"by": dim.as_str(), "rows": rows}).to_string())
    }

    async fn do_cost_forecast(&self, pid: &str) -> Result<String, String> {
        let f = self
            .provision
            .rollup_repo()
            .latest_forecast(pid)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&f).unwrap_or_default())
    }

    async fn do_cost_anomalies(&self, a: CostAnomaliesArgs) -> Result<String, String> {
        let rows = self
            .provision
            .rollup_repo()
            .anomalies(a.project.as_deref(), a.limit.unwrap_or(50))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&rows).unwrap_or_default())
    }

    async fn do_cost_tree(&self, a: AsOfArgs) -> Result<String, String> {
        let as_of = a.as_of.unwrap_or_else(asgard_provision::today);
        let tree = self
            .provision
            .cost_tree(&self.registry, &as_of, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&tree).unwrap_or_default())
    }

    async fn do_cost_movers(&self, a: CostMoversArgs) -> Result<String, String> {
        let as_of = a.as_of.unwrap_or_else(asgard_provision::today);
        let movers = self
            .provision
            .cost_movers(&as_of, a.top.unwrap_or(5) as usize, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&movers).unwrap_or_default())
    }

    async fn do_governance_metrics(&self) -> Result<String, String> {
        let metrics = self
            .registry
            .governance_metrics(None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&metrics).unwrap_or_default())
    }

    async fn do_request_resource(
        &self,
        pid: &str,
        a: RequestResourceArgs,
    ) -> Result<String, String> {
        let spec = a.spec.unwrap_or_else(|| json!({}));
        let requester = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let outcome = self
            .provision
            .request(
                &self.workflow,
                &self.registry,
                pid,
                &a.resource_type,
                &a.name,
                spec,
                requester,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&outcome).unwrap_or_default())
    }

    async fn do_request_promotion(
        &self,
        pid: &str,
        target: &str,
        actor: &str,
    ) -> Result<String, String> {
        let req = self
            .registry
            .request_promotion(&self.workflow, pid, target, actor)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&req).unwrap_or_default())
    }

    async fn do_promotion_status(&self, pid: &str) -> Result<String, String> {
        let checklist = self
            .registry
            .promotion_checklist(pid)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&checklist).unwrap_or_default())
    }

    async fn do_deprovision(
        &self,
        scope: Option<&str>,
        a: DeprovisionArgs,
    ) -> Result<String, String> {
        // When authenticated, the resource must belong to the caller's project.
        if let Some(pid) = scope {
            let rec = self
                .provision
                .repo()
                .get(&a.resource_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("resource {} not found", a.resource_id))?;
            if rec.project_id != pid {
                return Err("cross-project access denied".into());
            }
        }
        let actor = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let record = self
            .provision
            .deprovision(&a.resource_id, actor)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&record).unwrap_or_default())
    }

    async fn do_get_secret(&self, pid: &str, a: SecretArgs) -> Result<String, String> {
        let caller = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let value = self
            .provision
            .get_secret(pid, &a.name, caller)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({"name": a.name, "value": value}).to_string())
    }

    async fn do_rotate_secret(&self, pid: &str, a: SecretArgs) -> Result<String, String> {
        let caller = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let sref = self
            .provision
            .rotate_secret(pid, &a.name, caller)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&sref).unwrap_or_default())
    }

    async fn do_list_secrets(&self, pid: &str) -> Result<String, String> {
        let list = self
            .provision
            .list_secrets(pid)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&list).unwrap_or_default())
    }

    fn do_seed_list(&self) -> Result<String, String> {
        let list: Vec<_> = seed::all()
            .iter()
            .map(|m| {
                json!({
                    "id": m.id, "title": m.title, "kind": m.kind.as_str(), "path": m.path,
                    "tier": m.tier.as_str(), "languages": m.languages, "summary": m.summary,
                })
            })
            .collect();
        Ok(serde_json::to_string(&list).unwrap_or_default())
    }

    fn do_seed_plan(&self, a: SeedPlanArgs) -> Result<String, String> {
        let tier = match a.tier.as_deref() {
            Some(t) => seed::SeedTier::parse(t)
                .ok_or_else(|| "tier must be minimal, standard, or strict".to_string())?,
            None => seed::SeedTier::Standard,
        };
        let langs = a.languages.unwrap_or_default();
        let task = a.task.unwrap_or_default();
        let files: Vec<_> = seed::plan(&langs, &task, tier)
            .iter()
            .map(|m| {
                json!({"id": m.id, "path": m.path, "kind": m.kind.as_str(), "summary": m.summary})
            })
            .collect();
        Ok(json!({
            "tier": tier.as_str(),
            "languages": langs,
            "task": task,
            "files": files,
            "next": "for each file, call seed_get(id) to fetch its body and write it to the given path",
        })
        .to_string())
    }

    /// One-shot seed: the same plan as `seed_plan`, but each file's `body`
    /// inlined so the agent writes the whole starting point (AGENTS.md + the
    /// `.agent/` standards) in a single call instead of looping `seed_get`.
    fn do_bootstrap(&self, a: SeedPlanArgs) -> Result<String, String> {
        let tier = match a.tier.as_deref() {
            Some(t) => seed::SeedTier::parse(t)
                .ok_or_else(|| "tier must be minimal, standard, or strict".to_string())?,
            None => seed::SeedTier::Standard,
        };
        let langs = a.languages.unwrap_or_default();
        let task = a.task.unwrap_or_default();
        let files: Vec<_> = seed::plan(&langs, &task, tier)
            .iter()
            .map(|m| json!({"path": m.path, "title": m.title, "body": m.body}))
            .collect();
        Ok(json!({
            "tier": tier.as_str(),
            "files": files,
            "next": "write each file to its path (create directories as needed), then call register_project",
        })
        .to_string())
    }

    fn do_seed_get(&self, id: &str) -> Result<String, String> {
        match seed::get(id) {
            Some(m) => Ok(json!({
                "id": m.id, "path": m.path, "kind": m.kind.as_str(),
                "title": m.title, "body": m.body,
            })
            .to_string()),
            None => Err(format!(
                "unknown seed module '{id}'; call seed_list or seed_plan for ids"
            )),
        }
    }
}

// --- MCP tool surface (thin wrappers over the domain layer) ------------------

fn ok_text(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn wrap(r: Result<String, String>) -> Result<CallToolResult, McpError> {
    match r {
        Ok(s) => ok_text(s),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
    }
}

fn deny(e: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(e)]))
}

#[tool_router]
impl AsgardMcp {
    #[tool(description = "Search the entity catalog by kind and/or query.")]
    async fn catalog_search(
        &self,
        Parameters(a): Parameters<CatalogSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_catalog_search(a).await)
    }

    #[tool(description = "Fetch a single entity by kind/namespace/name.")]
    async fn catalog_get(
        &self,
        Parameters(a): Parameters<CatalogGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_catalog_get(a).await)
    }

    #[tool(
        description = "Discover the service catalog: every service an agent can provision, as machine-readable manifests (id, category, status, classification range, cost model, provisioner connector, required fields)."
    )]
    async fn list_services(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_list_services().await)
    }

    #[tool(description = "Fetch one service manifest by id (see list_services for ids).")]
    async fn get_service(
        &self,
        Parameters(a): Parameters<IdArg>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_get_service(&a.id))
    }

    #[tool(
        description = "Register a project (the mandatory gate). Mints a stable proj-YYYY-NNNN id. On a user-token connection the owner is stamped from your identity (owner_email is ignored). manager_email and group are required or optional per the operator's policy (see list_groups and registration policy); a blank manager defaults to the owner."
    )]
    async fn register_project(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<RegisterProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        // A user PAT registers projects it owns; the owner is its identity, not a
        // spoofable argument. A project key / stdio uses the supplied owner_email.
        let owner_override = match Self::auth(&ctx) {
            Some(McpAuth::User { email, .. }) => Some(email),
            _ => None,
        };
        wrap(self.do_register_project(a, owner_override).await)
    }

    #[tool(
        description = "Update a project's classification evidence record (governance metadata: owners, support/runbook/monitoring URLs, security review status, maintainers, data flows, etc.). PUT semantics — the submitted fields replace the stored evidence block; omitted fields are cleared. Requires a project you own/manage."
    )]
    async fn update_project(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<UpdateProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        let actor = match Self::auth(&ctx) {
            Some(McpAuth::User { email, .. }) => format!("user:{email}"),
            Some(McpAuth::Project { project_id }) => format!("project:{project_id}"),
            None => DEFAULT_REQUESTER.to_string(),
        };
        wrap(self.do_update_project(&pid, a.evidence, &actor).await)
    }

    #[tool(
        description = "Fetch a registered project's record (owner, manager, group, cost-center, classification, budget, spend, lifecycle)."
    )]
    async fn project_get(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_project_get(&pid).await)
    }

    #[tool(description = "Read a project's runtime state (budget, spend, kill switch).")]
    async fn project_state(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_project_state(&pid).await)
    }

    #[tool(
        description = "Issue a per-project gateway virtual key (the project's LLM credential). Requires a registered, active project you own/manage. Use this key out-of-band against the gateway endpoint (/api/gateway/chat) — inference is service usage, not a control-plane call."
    )]
    async fn gateway_credential(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectIdRequired>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, Some(a.project_id)).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_gateway_credential(&pid).await)
    }

    #[tool(
        description = "List the cost-centers / groups a project may register against (operator-configured allowlist)."
    )]
    async fn list_groups(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_list_groups())
    }

    #[tool(description = "List the enterprise standard sets an agent's output must conform to.")]
    async fn list_standards(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_list_standards().await)
    }

    #[tool(description = "Fetch the full text of a standard set by id (see list_standards).")]
    async fn get_standards(
        &self,
        Parameters(a): Parameters<IdArg>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_get_standards(&a.id).await)
    }

    #[tool(
        description = "List guidance — governed how-to playbooks (advisory, runtime-editable) for doing things well on this platform. Returns slug, title, summary, tags, and body."
    )]
    async fn guidance_list(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_guidance_list().await)
    }

    #[tool(description = "Fetch one guidance doc by slug (see guidance_list).")]
    async fn guidance_get(
        &self,
        Parameters(a): Parameters<GuidanceGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_guidance_get(&a.slug).await)
    }

    #[tool(
        description = "Create or update a guidance doc (markdown body). Optional category facet: best-practice, guide (default), or reference. Reusing a slug updates it — so an agent can write down a playbook it learned for the next agent. Submissions are drafts until an admin approves them."
    )]
    async fn guidance_put(
        &self,
        Parameters(a): Parameters<GuidancePutArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_guidance_put(a).await)
    }

    #[tool(
        description = "List recipes — narrated runbooks for building and deploying a whole capability on the platform (e.g. real-time collaboration, an MCP server). Each `body` is a rich markdown guide you follow end to end; an at-a-glance `spec` of steps supplements it. The control plane does not execute recipes — you read and follow them."
    )]
    async fn recipe_list(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_recipe_list().await)
    }

    #[tool(
        description = "Fetch one recipe by slug (see recipe_list). Returns the full markdown runbook (`body`) — what to build, the image + env contract, the ordered request_resource calls, how to verify, gotchas — plus an at-a-glance `spec`."
    )]
    async fn recipe_get(
        &self,
        Parameters(a): Parameters<RecipeGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_recipe_get(&a.slug).await)
    }

    #[tool(
        description = "Create or update a recipe (spec = { description, inputs, steps, outputs }). Reusing a slug updates it — capture a composition you proved so the next agent can reuse it."
    )]
    async fn recipe_put(
        &self,
        Parameters(a): Parameters<RecipePutArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_recipe_put(a).await)
    }

    #[tool(
        description = "Model/token spend rolled up by a dimension: project, owner, manager, group, classification, model, or provider."
    )]
    async fn cost_report(
        &self,
        Parameters(a): Parameters<CostReportArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_report(a).await)
    }

    #[tool(
        description = "Full cost picture for one project: model spend to date, infrastructure estimate per live resource, and the backend's measured actual. Window defaults to month-to-date."
    )]
    async fn project_cost(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectCostArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_project_cost(&pid, a).await)
    }

    #[tool(
        description = "Persisted daily cost rollup for one project: per-day actual delta, MTD cumulative, and estimate. Defaults to month-to-date."
    )]
    async fn cost_series(
        &self,
        Parameters(a): Parameters<CostSeriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_series(a).await)
    }

    #[tool(
        description = "Rollup spend (actual + estimate) grouped by a dimension: project, owner, manager, group, cost_center, classification, or service. Defaults to month-to-date."
    )]
    async fn cost_by(
        &self,
        Parameters(a): Parameters<CostByArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_by(a).await)
    }

    #[tool(
        description = "Latest end-of-month spend forecast for a project (linreg over month-to-date cumulative, with a confidence band)."
    )]
    async fn cost_forecast(
        &self,
        Parameters(a): Parameters<ProjectIdRequired>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_forecast(&a.project_id).await)
    }

    #[tool(
        description = "Recent cost anomalies (a day's spend far from a source's trailing norm), newest first. Optionally filtered to one project."
    )]
    async fn cost_anomalies(
        &self,
        Parameters(a): Parameters<CostAnomaliesArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_anomalies(a).await)
    }

    #[tool(
        description = "Org-cost tree for the month: company to group to manager to owner to project, each node with MTD, EOM forecast (± band), budget, and budget pressure."
    )]
    async fn cost_tree(
        &self,
        Parameters(a): Parameters<AsOfArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_tree(a).await)
    }

    #[tool(
        description = "Top movers: biggest absolute percent change in MTD spend versus the previous month, by project and by group."
    )]
    async fn cost_movers(
        &self,
        Parameters(a): Parameters<CostMoversArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_cost_movers(a).await)
    }

    #[tool(
        description = "Org-wide governance / portfolio metrics: owner-less and support-path gaps on operational systems, under-staffed Wide/Critical systems, stale POCs, expired-review inventory, unsupported-stack count, and Light-operational promotion cycle time. Metrics with no data source yet are labelled, not reported as zero. Each count carries its offending project ids."
    )]
    async fn governance_metrics(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_governance_metrics().await)
    }

    #[tool(
        description = "Request an infrastructure resource for a registered project (e.g. s3-bucket, dynamodb-table, random-secret). Self-service types provision immediately; review-tier types await approval."
    )]
    async fn request_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<RequestResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_request_resource(&pid, a).await)
    }

    #[tool(
        description = "Read the promotion checklist for a project: its current tier, the one tier it may move to, and the evidence verdict (missing required fields + exception signals) for that move. Use this to see what to close before requesting a promotion."
    )]
    async fn promotion_status(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_promotion_status(&pid).await)
    }

    #[tool(
        description = "Request a one-step classification promotion (e.g. poc → light-operational). A clean Light target with complete evidence and no exception auto-approves; Wide/Critical targets and any exception route to a human reviewer. Returns the workflow request (approved or pending)."
    )]
    async fn request_promotion(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<PromotionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        let actor = match Self::auth(&ctx) {
            Some(McpAuth::User { email, .. }) => format!("user:{email}"),
            Some(McpAuth::Project { project_id }) => format!("project:{project_id}"),
            None => DEFAULT_REQUESTER.to_string(),
        };
        wrap(self.do_request_promotion(&pid, &a.target, &actor).await)
    }

    #[tool(
        description = "Tear down a provisioned resource (routes to the manifest's connector destroy and marks the record destroyed). Use the resource id from a request_resource outcome."
    )]
    async fn deprovision_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<DeprovisionArgs>,
    ) -> Result<CallToolResult, McpError> {
        // A user PAT may tear down a resource in any project it owns/manages; a
        // project key is locked to its own project; stdio falls back to
        // default_project. In every case the resource must belong to the resolved
        // project — the resource id is not a cross-project handle.
        match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => {
                let rec = match self.provision.repo().get(&a.resource_id).await {
                    Ok(Some(r)) => r,
                    Ok(None) => return deny(format!("resource {} not found", a.resource_id)),
                    Err(e) => return deny(e.to_string()),
                };
                if let Err(e) = self.authorize_user(&email, &role, &rec.project_id).await {
                    return deny(e);
                }
                wrap(self.do_deprovision(None, a).await)
            }
            Some(McpAuth::Project { project_id }) => {
                wrap(self.do_deprovision(Some(&project_id), a).await)
            }
            None => {
                let scope = self.default_project.clone();
                wrap(self.do_deprovision(scope.as_deref(), a).await)
            }
        }
    }

    #[tool(
        description = "Fetch a secret value for a project (control plane: handing you a credential the control plane minted). Audited; the value is never logged. On a user token pass project_id; on a project key omit it. Use the secret name reported in a resource's outputs."
    )]
    async fn get_secret(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<SecretArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_get_secret(&pid, a).await)
    }

    #[tool(
        description = "Rotate a secret to a fresh value (new version, stable reference). Audited. On a user token pass project_id; on a project key omit it."
    )]
    async fn rotate_secret(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<SecretArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_rotate_secret(&pid, a).await)
    }

    #[tool(
        description = "List secret metadata (name, version, rotation) for a project. Never returns values. On a user token pass project_id; on a project key omit it."
    )]
    async fn list_secrets(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_list_secrets(&pid).await)
    }

    #[tool(
        description = "List every available agent-seed module (core operating agreement, per-language add-ons, domain overlays, artifact templates) with its kind, suggested repo path, and what it covers."
    )]
    async fn seed_list(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_seed_list())
    }

    #[tool(
        description = "Plan the agent-seed for this repo: given the languages it is written in and a description of the work, return the minimal relevant set of seed files to add (core + matching language add-ons + matching domain overlays + relevant templates) — not a one-shot dump. Each entry has an id and the path to write it to; fetch bodies with seed_get."
    )]
    async fn seed_plan(
        &self,
        Parameters(a): Parameters<SeedPlanArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_seed_plan(a))
    }

    #[tool(
        description = "Fetch one agent-seed module's full markdown body and its suggested repo path (use the ids from seed_plan / seed_list). Write the body to the path to seed the repo."
    )]
    async fn seed_get(&self, Parameters(a): Parameters<IdArg>) -> Result<CallToolResult, McpError> {
        wrap(self.do_seed_get(&a.id))
    }

    #[tool(
        description = "Set up a repo on Asgard in one call: returns the agent-seed plan with every file's body inlined (AGENTS.md + the .agent/ coding and security standards for the repo's languages/work). Write each file to its path, then register_project. One shot — no seed_plan/seed_get loop."
    )]
    async fn bootstrap(
        &self,
        Parameters(a): Parameters<SeedPlanArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_bootstrap(a))
    }
}

#[prompt_router]
impl AsgardMcp {
    /// Surfaced as a slash command by MCP clients (e.g. Claude Code's
    /// `/mcp__asgard__bootstrap`): a one-line shortcut that tells the agent to run
    /// the seed → register loop. No arguments, so it expands without prompting.
    #[prompt(
        name = "bootstrap",
        description = "Set up the current repo on Asgard: pull the AGENTS.md starting point + coding/security standards, then register the project."
    )]
    async fn bootstrap_prompt(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "Set this repository up on Asgard. Call the `bootstrap` tool to fetch the \
             AGENTS.md starting point and the `.agent/` coding and security standards, \
             write each returned file to its path (create directories as needed), then \
             call `register_project` to register this project — ask me for the owner, \
             budget, and data classification if you don't already have them.",
        )]
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for AsgardMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .build();
        info.server_info = Implementation::new("asgard", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Governance control plane. Connect with a user token to register projects \
             and manage every project you own/manage, or a project key scoped to \
             one. Register a project (the gate), then provision services and fetch \
             the credentials the control plane mints (e.g. gateway_credential for the LLM key). \
             Using a service — calling an LLM, reading a bucket — is out-of-band \
             with that service's own credential, not a control-plane tool. Call \
             list_services and list_standards to discover the catalog."
                .to_string(),
        );
        info
    }
}

fn month_start(day: &str) -> String {
    format!("{}-01", &day[..7.min(day.len())])
}

// --- HTTP transport (Streamable HTTP at /mcp, bearer-key gated) --------------

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;

/// Serve the MCP handler over stdio (for `asgard mcp` / local clients). Blocks
/// until the peer disconnects.
pub async fn serve_stdio(
    server: AsgardMcp,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// State for the `/mcp` auth middleware: a PAT (`asg_pat_…`) resolves to a user
/// principal via [`IdentityService`]; any other bearer is a project virtual key
/// resolved via [`GatewayRepo`].
#[derive(Clone)]
struct McpAuthState {
    gateway_repo: GatewayRepo,
    identity: IdentityService,
}

/// Build the axum router that serves the MCP endpoint at `/mcp` over Streamable
/// HTTP, gated by a bearer credential. A fresh [`AsgardMcp`] is built per
/// session; the authenticated principal arrives per-request via the middleware.
#[allow(clippy::too_many_arguments)]
pub fn http_router(
    catalog: CatalogRepo,
    gateway: Arc<Gateway>,
    registry: ProjectRegistry,
    workflow: Arc<WorkflowEngine>,
    provision: ProvisionService,
    gateway_repo: GatewayRepo,
    identity: IdentityService,
) -> Router {
    let factory = move || {
        Ok(AsgardMcp::new(
            catalog.clone(),
            gateway.clone(),
            registry.clone(),
            workflow.clone(),
            provision.clone(),
            None,
        ))
    };
    // The deployment sits behind the operator's own ingress/TLS, which is the
    // trust boundary; the SDK's default Host allowlist (localhost only) would
    // reject requests to https://<host>/mcp, so disable the rebinding guard.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let svc = StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);
    let auth_state = McpAuthState {
        gateway_repo,
        identity,
    };
    Router::new()
        .route_service("/mcp", svc)
        .layer(from_fn_with_state(auth_state, mcp_auth))
}

async fn mcp_auth(State(st): State<McpAuthState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing bearer credential").into_response();
    };
    // A user PAT is prefix-distinct from a project key; resolve it to a user
    // principal. Anything else is treated as a project virtual key.
    if token.starts_with(PAT_PREFIX) {
        return match st.identity.validate_pat(&token).await {
            Ok(user) => {
                req.extensions_mut().insert(McpAuth::User {
                    email: user.email.unwrap_or_default(),
                    role: user.role,
                });
                next.run(req).await
            }
            Err(_) => (StatusCode::UNAUTHORIZED, "invalid or revoked user token").into_response(),
        };
    }
    match st.gateway_repo.verify_key(&token).await {
        Ok(Some(project_id)) => {
            req.extensions_mut().insert(McpAuth::Project { project_id });
            next.run(req).await
        }
        Ok(None) => (StatusCode::UNAUTHORIZED, "invalid or revoked gateway key").into_response(),
        Err(e) => {
            tracing::warn!("mcp auth verify_key failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "auth check failed").into_response()
        }
    }
}

#[cfg(test)]
impl AsgardMcp {
    /// Test shim mirroring [`AsgardMcp::resolve_project`] without a live HTTP
    /// request context: `auth` is the authenticated principal (if any), `arg` the
    /// tool's project_id argument.
    async fn resolve_principal_for_test(
        &self,
        auth: Option<McpAuth>,
        arg: Option<String>,
    ) -> Result<String, String> {
        let arg = arg.filter(|s| !s.is_empty());
        match auth {
            Some(McpAuth::Project { project_id }) => {
                if let Some(p) = arg {
                    if p != project_id {
                        return Err("cross-project access denied".into());
                    }
                }
                Ok(project_id)
            }
            Some(McpAuth::User { email, role }) => {
                let pid =
                    arg.ok_or_else(|| "project_id is required for a user token".to_string())?;
                self.authorize_user(&email, &role, &pid).await.map(|_| pid)
            }
            None => arg
                .or_else(|| self.default_project.clone())
                .ok_or_else(|| "project_id required (no authenticated project)".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use asgard_catalog::{Entity, Manifest, Metadata, Origin};
    use asgard_gateway::{MockProvider, Mode, ModelInfo, ModelRegistry, Provider};
    use asgard_policy::{CedarEngine, PolicyEngine};
    use asgard_provision::{ProvisionRepo, ProvisionService};
    use asgard_registry::{GroupAllowlist, GroupEntry, RegistrationPolicy};
    use asgard_storage::Db;

    async fn server(default_project: Option<String>) -> AsgardMcp {
        let path =
            std::env::temp_dir().join(format!("asgard-mcp-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();

        let catalog = CatalogRepo::new(db.clone());
        let m = Manifest {
            api_version: Some("asgard.dev/v1".into()),
            kind: "Agent".into(),
            metadata: Metadata {
                name: "code-reviewer".into(),
                namespace: "default".into(),
                title: Some("Code Reviewer".into()),
                ..Default::default()
            },
            spec: json!({"owner": "group:default/platform", "model": "model:default/mock"}),
            relations: vec![],
        };
        catalog
            .upsert(&Entity::from_manifest(m, Origin::default()))
            .await
            .unwrap();

        let repo = GatewayRepo::new(db.clone());
        let model_registry = ModelRegistry::from_models(vec![ModelInfo {
            model_ref: "model:default/mock".into(),
            provider: "mock".into(),
            route_model: "mock".into(),
            data_classes: vec!["internal".into()],
            cost_in: 1.0,
            cost_out: 1.0,
        }]);
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(MockProvider));
        let policy: Arc<dyn PolicyEngine> = Arc::new(CedarEngine::new().unwrap());
        let gw = Arc::new(Gateway::new(
            repo.clone(),
            policy.clone(),
            model_registry,
            providers,
            Mode::Enforce,
        ));
        let allow = GroupAllowlist::new(vec![GroupEntry {
            key: "platform".into(),
            display_name: "Platform".into(),
            cost_center: "CC-100".into(),
            active: true,
        }]);
        let registry = ProjectRegistry::new(
            db.clone(),
            repo,
            catalog.clone(),
            allow,
            RegistrationPolicy::default(),
        );
        registry.seed_knowledge().await.unwrap();
        let workflow = Arc::new(WorkflowEngine::new(db.clone(), policy));
        let provision = ProvisionService::new(ProvisionRepo::new(db));
        AsgardMcp::new(catalog, gw, registry, workflow, provision, default_project)
    }

    async fn register(s: &AsgardMcp) -> String {
        let out = s
            .do_register_project(
                RegisterProjectArgs {
                    name: "Test".into(),
                    owner_email: "a@corp.example".into(),
                    manager_email: "b@corp.example".into(),
                    group: "platform".into(),
                    classification: None,
                    data_class: None,
                    budget_usd: None,
                    description: None,
                    requester: None,
                    evidence: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["project_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn get_info_declares_tools_and_prompts() {
        let s = server(None).await;
        let info = s.get_info();
        assert_eq!(info.server_info.name, "asgard");
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_some());
    }

    #[tokio::test]
    async fn bootstrap_inlines_agents_md_and_standards() {
        let s = server(None).await;
        let out = s
            .do_bootstrap(SeedPlanArgs {
                languages: Some(vec!["rust".into()]),
                task: Some("build a service".into()),
                tier: None,
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let files = v["files"].as_array().unwrap();
        // AGENTS.md is always included, with its body inlined (no second fetch).
        let agents = files
            .iter()
            .find(|f| f["path"] == "AGENTS.md")
            .expect("AGENTS.md in plan");
        assert!(agents["body"].as_str().unwrap().contains("AGENTS.md"));
        // The Rust add-on is pulled in for a Rust repo.
        assert!(files.iter().any(|f| f["path"] == ".agent/lang/RUST.md"));
    }

    #[tokio::test]
    async fn catalog_search_returns_entity() {
        let s = server(None).await;
        let text = s
            .do_catalog_search(CatalogSearchArgs {
                kind: Some("Agent".into()),
                query: None,
            })
            .await
            .unwrap();
        assert!(text.contains("agent:default/code-reviewer"));
    }

    #[tokio::test]
    async fn list_services_exposes_catalog() {
        let s = server(None).await;
        let text = s.do_list_services().await.unwrap();
        assert!(text.contains("s3-bucket"));
        assert!(text.contains("\"connector\":\"terraform\""));
    }

    #[tokio::test]
    async fn register_then_credential() {
        // The control plane issues the project's LLM key; inference itself is
        // service usage (over the REST gateway), not an MCP tool.
        let s = server(None).await;
        let pid = register(&s).await;
        let cred = s.do_gateway_credential(&pid).await.unwrap();
        let key = serde_json::from_str::<serde_json::Value>(&cred).unwrap()["key"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(key.starts_with("asg_"));
    }

    #[tokio::test]
    async fn unregistered_project_cannot_get_credential() {
        let s = server(None).await;
        let err = s.do_gateway_credential("proj-2026-9999").await.unwrap_err();
        assert!(err.contains("not registered"));
    }

    #[tokio::test]
    async fn agent_loop_register_request_resource_cost() {
        let s = server(None).await;
        assert!(s.do_list_standards().await.unwrap().contains("coding"));
        let pid = register(&s).await;
        let text = s
            .do_request_resource(
                &pid,
                RequestResourceArgs {
                    project_id: Some(pid.clone()),
                    resource_type: "s3-bucket".into(),
                    name: "assets".into(),
                    spec: Some(json!({"name": "assets"})),
                    requester: Some("agent:default/builder".into()),
                },
            )
            .await
            .unwrap();
        let outcome: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(outcome["request"]["state"], json!("fulfilled"));
        assert_eq!(outcome["provisioned"]["tags"]["project"], json!(pid));

        let cost = s
            .do_cost_report(CostReportArgs {
                by: Some("group".into()),
                since: None,
                until: None,
            })
            .await
            .unwrap();
        assert!(cost.contains("\"by\":\"group\""));
    }

    #[tokio::test]
    async fn secret_roundtrip_scoped_to_project() {
        let s = server(Some("placeholder".into())).await;
        let pid = register(&s).await;
        s.do_request_resource(
            &pid,
            RequestResourceArgs {
                project_id: Some(pid.clone()),
                resource_type: "random-secret".into(),
                name: "api-key".into(),
                spec: Some(json!({"name": "api-key"})),
                requester: Some("agent:default/builder".into()),
            },
        )
        .await
        .unwrap();
        let got = s
            .do_get_secret(
                &pid,
                SecretArgs {
                    project_id: None,
                    name: "api-key-value".into(),
                    requester: None,
                },
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert!(v["value"].as_str().is_some_and(|x| !x.is_empty()));
    }

    #[tokio::test]
    async fn cross_project_secret_is_denied() {
        // With an authenticated project A, resolve_project rejects a request that
        // names project B — the project id is not a spoofable argument.
        let s = server(Some("proj-A".into())).await;
        let pid = s
            .resolve_principal_for_test(
                Some(McpAuth::Project {
                    project_id: "proj-A".into(),
                }),
                Some("proj-A".into()),
            )
            .await
            .unwrap();
        assert_eq!(pid, "proj-A");
        let denied = s
            .resolve_principal_for_test(
                Some(McpAuth::Project {
                    project_id: "proj-A".into(),
                }),
                Some("proj-B".into()),
            )
            .await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn user_token_authorizes_owned_project_only() {
        let s = server(None).await;
        // Register a project owned by alice (manager bob).
        let pid = register(&s).await;
        let alice = McpAuth::User {
            email: "a@corp.example".into(),
            role: "member".into(),
        };
        // Owner is authorized for their project.
        assert_eq!(
            s.resolve_principal_for_test(Some(alice.clone()), Some(pid.clone()))
                .await
                .unwrap(),
            pid
        );
        // A user token must name a project (no implicit single project).
        assert!(s
            .resolve_principal_for_test(Some(alice.clone()), None)
            .await
            .is_err());
        // A stranger (member role) is denied an unowned project.
        let stranger = McpAuth::User {
            email: "stranger@corp.example".into(),
            role: "member".into(),
        };
        assert!(s
            .resolve_principal_for_test(Some(stranger), Some(pid.clone()))
            .await
            .is_err());
        // An admin (see-all) is authorized for any project.
        let admin = McpAuth::User {
            email: "admin@corp.example".into(),
            role: "admin".into(),
        };
        assert_eq!(
            s.resolve_principal_for_test(Some(admin), Some(pid.clone()))
                .await
                .unwrap(),
            pid
        );
    }
}
