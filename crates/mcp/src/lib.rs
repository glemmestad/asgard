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
//! project comes from `FRONTKEEP_PROJECT` (local trust), passed as `default_project`.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use asgard_catalog::{seed, CatalogRepo, ListFilter};
use asgard_gateway::{Gateway, GatewayRepo};
use asgard_identity::{IdentityService, Role, PAT_PREFIX};
use asgard_provision::{ProvisionService, RollupDim};
use asgard_registry::{CostDim, McpServerInput, ProjectRegistry, RegisterInput, SkillInput};
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
    /// stdio/local fallback project (from `FRONTKEEP_PROJECT`); `None` over HTTP,
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
    /// Adopt an existing (brownfield) system: registers in the `provisional`
    /// lifecycle — fully live (keys, resources, cost attribution) but flagged
    /// for triage until its first promotion flips it to `active`.
    #[serde(default)]
    pub provisional: bool,
    #[serde(flatten)]
    pub evidence: asgard_registry::Evidence,
}

/// Project id plus mutable fields. `name`/`description`/`budget_usd` patch the
/// project in place (id unchanged). The evidence block keeps PUT semantics, but
/// is only written when a field is supplied — a name-only update leaves evidence
/// intact.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateProjectArgs {
    pub project_id: Option<String>,
    /// New display name (e.g. code-name → production name). Id stays the same.
    pub name: Option<String>,
    pub description: Option<String>,
    /// New monthly budget. Up to the classification's self-service ceiling applies
    /// immediately; above it routes to human review.
    pub budget_usd: Option<f64>,
    /// Transfer the project to a new owner (the owner gains authority + cost scoping).
    /// Allowed for the current owner/manager or an admin.
    pub owner_email: Option<String>,
    /// Reassign the project's manager.
    pub manager_email: Option<String>,
    #[serde(flatten)]
    pub evidence: asgard_registry::Evidence,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdArg {
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPendingApprovalsArgs {
    /// Optional: narrow to one project id. Omit to list every pending request you may see.
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApprovalDecisionArgs {
    /// The pending request id (from list_pending_approvals).
    pub request_id: String,
    /// Optional note recorded with the decision.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct McpCatalogGetArgs {
    /// The catalog entry id (see mcp_catalog_list).
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct McpCatalogPublishArgs {
    /// Provide to update an existing entry you own; omit to publish a new one.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    /// Optional rich getting-started / README (markdown).
    #[serde(default)]
    pub readme: String,
    /// Structured install: { transport: "stdio"|"remote", command, args, env, url }.
    /// stdio uses command/args/env (env is a list of variable names); remote uses url.
    #[serde(default)]
    pub install: serde_json::Value,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct McpCatalogStateArgs {
    pub id: String,
    /// `active`, `disabled` (temporarily hide), or `archived` (retire).
    pub state: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillsCatalogGetArgs {
    /// The skill id (see skills_catalog_list).
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillsCatalogPublishArgs {
    /// Provide to update an existing skill you own; omit to publish a new one.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional — falls back to the SKILL.md frontmatter `name`.
    #[serde(default)]
    pub name: String,
    /// Optional — falls back to the SKILL.md frontmatter `description`.
    #[serde(default)]
    pub summary: String,
    /// Optional extra getting-started notes (markdown), beyond the SKILL.md.
    #[serde(default)]
    pub readme: String,
    /// Authored runtime: `claude-code` (default) or `codex`.
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The skill's file tree: `[{ path, content_b64 }]`. Must include a `SKILL.md`.
    pub bundle: Vec<asgard_skills::SkillFile>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillsCatalogStateArgs {
    pub id: String,
    /// `active`, `disabled` (temporarily hide), or `archived` (retire).
    pub state: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillsCatalogExportArgs {
    /// The skill id (see skills_catalog_list).
    pub id: String,
    /// Target runtime: `claude-code` or `codex`. Defaults to the skill's own runtime.
    #[serde(default)]
    pub runtime: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillsCatalogInstallArgs {
    /// The skill id (see skills_catalog_list).
    pub id: String,
    /// Install destination: `claude-code`, `codex`, or `cursor`. Defaults to the
    /// skill's own runtime.
    #[serde(default)]
    pub dest: Option<String>,
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

/// schemars renders `serde_json::Value` as a boolean (`true`) schema, which some
/// MCP clients reject as a property-level input schema. Emit a free-form object.
fn freeform_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object" })
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
    #[schemars(schema_with = "freeform_object")]
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
    #[schemars(schema_with = "freeform_object")]
    pub spec: Option<serde_json::Value>,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinkResourceArgs {
    pub project_id: Option<String>,
    /// Human label for the linked infrastructure (e.g. "legacy RDS + S3").
    pub name: String,
    /// The cost source that attributes its spend (e.g. aws-cost-explorer,
    /// databricks-billing, litellm, flat, none).
    pub cost_source: String,
    /// Monthly estimate, reported until/unless the source measures actuals.
    pub est_monthly_usd: Option<f64>,
    /// Extra tags to record on the link (informational — the cloud-side
    /// `project=<id>` tag is what cost sources filter on).
    pub tags: Option<std::collections::BTreeMap<String, String>>,
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployImageArgs {
    /// Target project (required on a user token; omit on a project key).
    pub project_id: Option<String>,
    /// The provisioned container service to roll. Use an id from list_resources.
    pub resource_id: String,
    /// The new image reference, e.g. `<acct>.dkr.ecr.<region>.amazonaws.com/<repo>:<sha>`.
    pub image: String,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestGrantArgs {
    /// Target project (required on a user token; omit on a project key). Both
    /// resources must belong to it.
    pub project_id: Option<String>,
    /// The resource being granted access (e.g. an ecs-service); its identity gets
    /// the access. Use an id from list_resources.
    pub consumer_resource_id: String,
    /// The resource access is granted to (e.g. an s3-bucket).
    pub target_resource_id: String,
    /// Access level the target defines; defaults to `write` (read+write).
    #[serde(default)]
    pub level: Option<String>,
    pub requester: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListResourcesArgs {
    /// Target project (required on a user token; omit on a project key).
    pub project_id: Option<String>,
    /// Optional state filter (e.g. "provisioned", "destroyed", "suspended").
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetResourceArgs {
    /// The resource id from a request_resource outcome or list_resources.
    pub resource_id: String,
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
pub struct EscalatePromotionArgs {
    /// The flagged promotion request id (from `request_promotion`'s response).
    pub request_id: String,
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
pub struct RegistrationRequirementsArgs {
    /// Optional: one classification tier (e.g. "light-operational"). Omit for all tiers.
    pub classification: Option<String>,
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

    /// Every tool name the MCP server advertises, read from the live tool router
    /// (the same source `tools/list` serves to agents). The CLI↔MCP lock-step CI
    /// check asserts this set matches the CLI's typed surface — keep them in step.
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    fn auth(ctx: &RequestContext<RoleServer>) -> Option<McpAuth> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<McpAuth>().cloned())
    }

    /// The authenticated principal as a requester/actor string (`user:{email}` or
    /// `project:{id}`), or `None` on an unauthenticated stdio connection.
    fn requester_from_auth(ctx: &RequestContext<RoleServer>) -> Option<String> {
        match Self::auth(ctx) {
            Some(McpAuth::User { email, .. }) => Some(format!("user:{email}")),
            Some(McpAuth::Project { project_id }) => Some(format!("project:{project_id}")),
            None => None,
        }
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

    /// Register a project. `a.owner_email`/`a.manager_email` are pre-resolved by the
    /// tool wrapper from the principal (a user registers for themselves by default;
    /// an on-behalf registration names the owner and keeps the caller as manager).
    /// `actor_email` is the authenticated caller, recorded for audit.
    async fn do_register_project(
        &self,
        a: RegisterProjectArgs,
        actor_email: Option<String>,
    ) -> Result<String, String> {
        let requester = a.requester.clone();
        // No budget given → default to half the classification's self-service
        // ceiling, so a project never registers with a $0 (effectively no-cap) budget.
        let budget_usd = a.budget_usd.or_else(|| {
            let class = a.classification.as_deref().unwrap_or("poc");
            self.provision
                .auto_approve_ceiling(class)
                .map(|ceiling| ceiling / 2.0)
        });
        let input = RegisterInput {
            name: a.name,
            owner_email: a.owner_email,
            manager_email: a.manager_email,
            group: a.group,
            classification: a.classification,
            data_class: a.data_class,
            budget_usd,
            description: a.description,
            provisional: a.provisional,
            evidence: a.evidence,
        };
        let actor = actor_email
            .as_deref()
            .map(|e| format!("user:{e}"))
            .unwrap_or_else(|| {
                requester
                    .as_deref()
                    .unwrap_or("agent:default/unknown")
                    .to_string()
            });
        let reg = self
            .registry
            .register(input, &actor)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&reg).unwrap_or_default())
    }

    async fn do_update_project(
        &self,
        pid: &str,
        a: UpdateProjectArgs,
        actor: &str,
    ) -> Result<String, String> {
        // Evidence is PUT — but only when supplied, so a name/budget-only update
        // doesn't clear it.
        if a.evidence != asgard_registry::Evidence::default() {
            self.registry
                .update_evidence(pid, a.evidence, actor)
                .await
                .map_err(|e| e.to_string())?;
        }
        let reg = self.registry.get(pid).await.map_err(|e| e.to_string())?;
        let ceiling = reg
            .as_ref()
            .and_then(|r| self.provision.auto_approve_ceiling(&r.classification));
        let (reg, budget) = self
            .registry
            .update_project(
                &self.workflow,
                pid,
                asgard_registry::ProjectUpdate {
                    name: a.name,
                    description: a.description,
                    budget_usd: a.budget_usd,
                    owner_email: a.owner_email,
                    manager_email: a.manager_email,
                },
                ceiling,
                actor,
            )
            .await
            .map_err(|e| e.to_string())?;
        let pending = match budget {
            asgard_registry::BudgetOutcome::PendingReview(req) => Some(req),
            _ => None,
        };
        Ok(serde_json::to_string(&serde_json::json!({
            "project": reg,
            "budget_review": pending,
        }))
        .unwrap_or_default())
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

    async fn do_list_projects(&self, auth: Option<McpAuth>) -> Result<String, String> {
        let mut projects = self.registry.list().await.map_err(|e| e.to_string())?;
        match auth {
            Some(McpAuth::Project { project_id }) => {
                projects.retain(|p| p.project_id == project_id)
            }
            Some(McpAuth::User { email, role }) => {
                if !Role::parse(&role).can(asgard_identity::Capability::ViewAllCost) {
                    projects.retain(|p| p.owner == email || p.manager == email);
                }
            }
            None => {
                if let Some(dp) = &self.default_project {
                    projects.retain(|p| &p.project_id == dp);
                }
            }
        }
        Ok(serde_json::to_string(&projects).unwrap_or_default())
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

    fn do_registration_requirements(
        &self,
        a: RegistrationRequirementsArgs,
    ) -> Result<String, String> {
        use asgard_registry::{CLASSIFICATIONS, DATA_CLASSES};
        let tiers: Vec<&str> = match a.classification.as_deref() {
            Some(c) => match CLASSIFICATIONS.iter().find(|t| **t == c) {
                Some(t) => vec![*t],
                None => {
                    return Err(format!(
                        "unknown classification '{c}'; one of: {}",
                        CLASSIFICATIONS.join(", ")
                    ))
                }
            },
            None => CLASSIFICATIONS.to_vec(),
        };
        let reqs = self.registry.requirements();
        let policy = self.registry.policy();
        let classifications: Vec<_> = tiers
            .iter()
            .map(|c| {
                let ceiling = self.provision.auto_approve_ceiling(c);
                json!({
                    "classification": c,
                    "required_evidence": reqs.required_through(c),
                    "auto_approve_ceiling_usd": ceiling,
                    "default_budget_usd": ceiling.map(|x| x / 2.0),
                })
            })
            .collect();
        Ok(json!({
            "classifications": classifications,
            "registration": {
                "require_manager": policy.require_manager,
                "require_group": policy.require_group,
                "groups": "call list_groups for the allowed groups / cost-centers",
            },
            "data_classes": DATA_CLASSES,
            "budget_rule": "an omitted budget defaults to half the tier's ceiling; a budget change above the ceiling routes to human review",
        })
        .to_string())
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

    async fn do_mcp_catalog_list(&self) -> Result<String, String> {
        // Agents browse the live catalog (active entries, both trust tiers).
        let list = self
            .registry
            .mcp_server_list(None, None, Some("active"))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    async fn do_mcp_catalog_get(&self, id: &str) -> Result<String, String> {
        match self
            .registry
            .mcp_server_get(id)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(m) => serde_json::to_string(&m).map_err(|e| e.to_string()),
            None => Err(format!(
                "unknown catalog entry '{id}'; call mcp_catalog_list for ids"
            )),
        }
    }

    /// Publish (or update) a catalog entry as the authenticated user. An admin's
    /// publish lands company-approved; everyone else's is user-submitted.
    async fn do_mcp_catalog_publish(
        &self,
        email: &str,
        role: &str,
        a: McpCatalogPublishArgs,
    ) -> Result<String, String> {
        let admin = Role::parse(role).can(asgard_identity::Capability::ManageUsers);
        let input = McpServerInput {
            name: a.name,
            summary: a.summary,
            readme: a.readme,
            install: a.install,
            repository: a.repository,
            homepage: a.homepage,
            version: a.version,
            tags: a.tags,
        };
        let m = match a.id.filter(|s| !s.is_empty()) {
            Some(id) => {
                let existing = self
                    .registry
                    .mcp_server_get(&id)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("unknown catalog entry '{id}'"))?;
                if existing.owner != email && !admin {
                    return Err("only the owner or an admin can edit this catalog entry".into());
                }
                self.registry
                    .mcp_server_update(&id, &input, email)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("unknown catalog entry '{id}'"))?
            }
            None => self
                .registry
                .mcp_server_create(email, &input, admin)
                .await
                .map_err(|e| e.to_string())?,
        };
        serde_json::to_string(&m).map_err(|e| e.to_string())
    }

    async fn do_mcp_catalog_set_state(
        &self,
        email: &str,
        role: &str,
        a: McpCatalogStateArgs,
    ) -> Result<String, String> {
        let admin = Role::parse(role).can(asgard_identity::Capability::ManageUsers);
        let existing = self
            .registry
            .mcp_server_get(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown catalog entry '{}'", a.id))?;
        if existing.owner != email && !admin {
            return Err("only the owner or an admin can change this catalog entry's state".into());
        }
        let action = if a.state == "active" {
            "enabled"
        } else {
            &a.state
        };
        self.registry
            .mcp_server_set_state(&a.id, &a.state, action, email)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&serde_json::json!({"ok": true, "id": a.id, "state": a.state}))
            .map_err(|e| e.to_string())
    }

    async fn do_skills_catalog_list(&self) -> Result<String, String> {
        let list = self
            .registry
            .skill_list(None, None, Some("active"))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    async fn do_skills_catalog_get(&self, id: &str) -> Result<String, String> {
        match self
            .registry
            .skill_get(id)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(s) => serde_json::to_string(&s).map_err(|e| e.to_string()),
            None => Err(format!(
                "unknown skill '{id}'; call skills_catalog_list for ids"
            )),
        }
    }

    /// Publish (or update) a skill as the authenticated user. An admin's publish
    /// lands company-approved; everyone else's is user-submitted.
    async fn do_skills_catalog_publish(
        &self,
        email: &str,
        role: &str,
        a: SkillsCatalogPublishArgs,
    ) -> Result<String, String> {
        let admin = Role::parse(role).can(asgard_identity::Capability::ManageUsers);
        let input = SkillInput {
            name: a.name,
            summary: a.summary,
            readme: a.readme,
            runtime: a.runtime,
            repository: a.repository,
            homepage: a.homepage,
            version: a.version,
            tags: a.tags,
            bundle: asgard_skills::SkillBundle { files: a.bundle },
        };
        let s = match a.id.filter(|s| !s.is_empty()) {
            Some(id) => {
                let existing = self
                    .registry
                    .skill_get(&id)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("unknown skill '{id}'"))?;
                if existing.owner != email && !admin {
                    return Err("only the owner or an admin can edit this skill".into());
                }
                self.registry
                    .skill_update(&id, &input, email)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("unknown skill '{id}'"))?
            }
            None => self
                .registry
                .skill_create(email, &input, admin)
                .await
                .map_err(|e| e.to_string())?,
        };
        serde_json::to_string(&s).map_err(|e| e.to_string())
    }

    async fn do_skills_catalog_set_state(
        &self,
        email: &str,
        role: &str,
        a: SkillsCatalogStateArgs,
    ) -> Result<String, String> {
        let admin = Role::parse(role).can(asgard_identity::Capability::ManageUsers);
        let existing = self
            .registry
            .skill_get(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown skill '{}'", a.id))?;
        if existing.owner != email && !admin {
            return Err("only the owner or an admin can change this skill's state".into());
        }
        let action = if a.state == "active" {
            "enabled"
        } else {
            &a.state
        };
        self.registry
            .skill_set_state(&a.id, &a.state, action, email)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&serde_json::json!({"ok": true, "id": a.id, "state": a.state}))
            .map_err(|e| e.to_string())
    }

    async fn do_skills_catalog_export(&self, a: SkillsCatalogExportArgs) -> Result<String, String> {
        let skill = self
            .registry
            .skill_get(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown skill '{}'", a.id))?;
        let blob = self
            .registry
            .skill_get_bundle(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown skill '{}'", a.id))?;
        let bundle = asgard_skills::from_json(&blob).map_err(|e| e.to_string())?;
        let from = asgard_skills::Runtime::parse(&skill.runtime).unwrap_or_default();
        let target = match a.runtime.as_deref() {
            Some(s) => asgard_skills::Runtime::parse(s)
                .ok_or_else(|| format!("unknown runtime '{s}' (expected claude-code or codex)"))?,
            None => from,
        };
        let res = asgard_skills::translate(&bundle, from, target).map_err(|e| e.to_string())?;
        serde_json::to_string(&serde_json::json!({
            "id": a.id,
            "name": skill.name,
            "origin": from.as_str(),
            "runtime": target.as_str(),
            "files": res.bundle.files,
            "loss": res.loss,
        }))
        .map_err(|e| e.to_string())
    }

    async fn do_skills_catalog_install(
        &self,
        a: SkillsCatalogInstallArgs,
    ) -> Result<String, String> {
        let skill = self
            .registry
            .skill_get(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown skill '{}'", a.id))?;
        let blob = self
            .registry
            .skill_get_bundle(&a.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown skill '{}'", a.id))?;
        let bundle = asgard_skills::from_json(&blob).map_err(|e| e.to_string())?;
        let dest_key = a.dest.as_deref().unwrap_or(&skill.runtime);
        let dest = asgard_skills::destination(dest_key).ok_or_else(|| {
            format!("unknown dest '{dest_key}' (expected claude-code, codex, or cursor)")
        })?;
        let from = asgard_skills::Runtime::parse(&skill.runtime).unwrap_or_default();
        let res =
            asgard_skills::translate(&bundle, from, dest.runtime).map_err(|e| e.to_string())?;
        let decoded = res.bundle.decoded().map_err(|e| e.to_string())?;
        let b64: std::collections::HashMap<&str, &str> = res
            .bundle
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.content_b64.as_str()))
            .collect();
        // Decoded text where possible so the agent writes plain files; base64 only for
        // the rare binary asset.
        let files: Vec<serde_json::Value> = decoded
            .iter()
            .map(|(path, bytes)| match std::str::from_utf8(bytes) {
                Ok(text) => {
                    serde_json::json!({ "path": path, "content": text, "encoding": "utf-8" })
                }
                Err(_) => serde_json::json!({
                    "path": path,
                    "content": b64.get(path.as_str()).copied().unwrap_or(""),
                    "encoding": "base64",
                }),
            })
            .collect();
        let dir = format!("{}/{}", dest.dir, asgard_skills::slug(&skill.name));
        serde_json::to_string(&serde_json::json!({
            "id": a.id,
            "name": skill.name,
            "dest": dest.key,
            "runtime": dest.runtime.as_str(),
            "dir": dir,
            "files": files,
            "loss": res.loss,
        }))
        .map_err(|e| e.to_string())
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

    async fn do_link_resource(&self, pid: &str, a: LinkResourceArgs) -> Result<String, String> {
        let reg = self
            .registry
            .require_active(pid)
            .await
            .map_err(|e| e.to_string())?;
        let rec = self
            .provision
            .link_resource(
                &reg,
                &a.name,
                &a.cost_source,
                a.est_monthly_usd.unwrap_or(0.0),
                a.tags.unwrap_or_default(),
                a.note.as_deref().unwrap_or(""),
            )
            .await
            .map_err(|e| e.to_string())?;
        let feed = if self.provision.cost_source_configured(a.cost_source.trim()) {
            ""
        } else {
            " This source has no live feed in this deployment yet, so the estimate stands in until an operator wires it."
        };
        Ok(json!({
            "resource": rec,
            "next": format!(
                "Frontkeep does not manage this infrastructure. Tag the underlying cloud resources \
                 `project={pid}` yourself — the cost source attributes spend by that tag. One link \
                 per cost source per project is enough.{feed} Unlink (record-only) with deprovision_resource."
            ),
        })
        .to_string())
    }

    async fn do_deploy_image(&self, pid: &str, a: DeployImageArgs) -> Result<String, String> {
        let requester = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let outcome = self
            .provision
            .deploy_image(
                &self.workflow,
                &self.registry,
                pid,
                &a.resource_id,
                &a.image,
                requester,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&outcome).unwrap_or_default())
    }

    async fn do_request_grant(&self, pid: &str, a: RequestGrantArgs) -> Result<String, String> {
        let level = a.level.as_deref().unwrap_or("write");
        let requester = a.requester.as_deref().unwrap_or(DEFAULT_REQUESTER);
        let outcome = self
            .provision
            .request_grant(
                &self.workflow,
                &self.registry,
                pid,
                &a.consumer_resource_id,
                &a.target_resource_id,
                level,
                requester,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string(&outcome).unwrap_or_default())
    }

    async fn do_list_resources(&self, pid: &str, state: Option<&str>) -> Result<String, String> {
        let mut recs = self
            .provision
            .repo()
            .list_by_project(pid)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(s) = state {
            recs.retain(|r| r.state == s);
        }
        Ok(serde_json::to_string(&recs).unwrap_or_default())
    }

    /// Pending (state=requested) requests the caller may *see*: scoped to projects they
    /// own or manage, unless they hold the see-all role. A project key sees only its own.
    async fn do_list_pending_approvals(
        &self,
        auth: Option<McpAuth>,
        project_id: Option<String>,
    ) -> Result<String, String> {
        // A request's project lives in its payload (provision/budget) or a `project:`
        // subject (promotion) — not in the workflow `subject` column — so scope in code
        // via WorkflowRequest::project_id rather than a SQL subject filter.
        let filter = asgard_workflow::RequestFilter {
            state: Some(asgard_workflow::State::Requested),
            ..Default::default()
        };
        let mut reqs = self
            .workflow
            .list(&filter)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(want) = project_id.filter(|s| !s.is_empty()) {
            reqs.retain(|r| r.project_id() == Some(want.as_str()));
        }
        match auth {
            Some(McpAuth::Project { project_id }) => {
                reqs.retain(|r| r.project_id() == Some(project_id.as_str()));
            }
            Some(McpAuth::User { email, role }) => {
                let see_all = Role::parse(&role).can(asgard_identity::Capability::ViewAllCost);
                if !see_all {
                    let projects = self.registry.list().await.map_err(|e| e.to_string())?;
                    let mine: std::collections::HashSet<String> = projects
                        .iter()
                        .filter(|p| p.owner == email || p.manager == email)
                        .map(|p| p.project_id.clone())
                        .collect();
                    reqs.retain(|r| r.project_id().is_some_and(|p| mine.contains(p)));
                }
            }
            None => {}
        }
        Ok(serde_json::to_string(&reqs).unwrap_or_default())
    }

    /// Approve (`approve=true`) or deny a pending request. The approver must be a user
    /// principal who is the subject project's manager or holds `ApproveRequests` — never
    /// the owner alone, never a project key (see `asgard_identity::may_approve_request`).
    /// On approval of a provisioning request, enqueue the apply (mirrors the REST path).
    async fn do_decide_request(
        &self,
        auth: Option<McpAuth>,
        id: &str,
        reason: Option<&str>,
        approve: bool,
    ) -> Result<String, String> {
        let (email, role) = match auth {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => {
                return Err("approving a request requires a user token (asg_pat_…); a project key cannot approve its own requests".into())
            }
        };
        let req = self
            .workflow
            .get(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("request {id} not found"))?;
        let manager = match req.project_id() {
            Some(pid) => self
                .registry
                .get(pid)
                .await
                .map_err(|e| e.to_string())?
                .map(|r| r.manager)
                .unwrap_or_default(),
            None => String::new(),
        };
        if !asgard_identity::may_approve_request(Role::parse(&role), &email, &manager) {
            return Err(format!(
                "not authorized to decide request {id} (its project's manager or an approver role only)"
            ));
        }
        let actor = format!("user:{email}");
        if !approve {
            let rejected = self
                .workflow
                .reject(id, &actor, reason)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(serde_json::to_string(&rejected).unwrap_or_default());
        }
        let approved = self
            .workflow
            .approve(id, &actor, reason)
            .await
            .map_err(|e| e.to_string())?;
        if approved.kind.starts_with("provision:")
            && approved.state == asgard_workflow::State::Approved
        {
            let outcome = self
                .provision
                .fulfill(&self.workflow, &self.registry, id, &actor)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(serde_json::to_string(&outcome.request).unwrap_or_default());
        }
        Ok(serde_json::to_string(&approved).unwrap_or_default())
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

    async fn do_escalate_promotion(&self, request_id: &str, actor: &str) -> Result<String, String> {
        let req = self
            .workflow
            .escalate(request_id, actor)
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
            "next": "Each entry's `body` is the actual file content. Write each NEW file verbatim to its `path` (create directories as needed) — actually create the files, do not summarize or just describe them. If a file already exists (an AGENTS.md or CLAUDE.md in an existing repo), do NOT overwrite it: merge — keep the repo's guidance and add the Frontkeep sections it lacks (the project id, the MCP tools, the gateway rule). The .agent/ files are additive. Then call register_project.",
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
        description = "Register a project (the mandatory gate). Mints a stable proj-YYYY-NNNN id. On a user-token connection you are the owner by default; to stand a project up for someone else, pass their owner_email — you are then recorded as the project's manager (so you keep authority), unless you are an admin, in which case you may hand off owner and manager outright. manager_email and group are optional per the operator's policy (see list_groups); a blank manager defaults to the owner. Call registration_requirements first for the evidence fields and budget ceilings per classification tier."
    )]
    async fn register_project(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(mut a): Parameters<RegisterProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve owner/manager from the principal: a user registers for themselves by
        // default. Naming a different owner_email is an on-behalf registration — an
        // admin may hand off ownership fully, while a non-admin stays the manager so
        // they retain authority over what they stood up. A project key / stdio uses the
        // supplied fields verbatim.
        let actor_email = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => {
                if a.owner_email.trim().is_empty() {
                    a.owner_email = email.clone();
                } else if !Role::parse(&role).is_admin() && a.manager_email.trim().is_empty() {
                    a.manager_email = email.clone();
                }
                Some(email)
            }
            _ => None,
        };
        wrap(self.do_register_project(a, actor_email).await)
    }

    #[tool(
        description = "Update a registered project in place — the project id never changes, so all tagging/cost attribution stays intact. Set `name` to relabel (code-name → production name), `budget_usd` to re-budget (up to the classification's self-service ceiling applies immediately; above it routes to human review), and/or the evidence fields to revise governance metadata (PUT — evidence is rewritten only when supplied, so a name/budget-only update leaves it intact). Requires a project you own/manage."
    )]
    async fn update_project(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<UpdateProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        let actor = match Self::auth(&ctx) {
            Some(McpAuth::User { email, .. }) => format!("user:{email}"),
            Some(McpAuth::Project { project_id }) => format!("project:{project_id}"),
            None => DEFAULT_REQUESTER.to_string(),
        };
        wrap(self.do_update_project(&pid, a, &actor).await)
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
        description = "List the projects you can see: every project you own or manage (admin/finance see all). A project key returns just its own project. No arguments."
    )]
    async fn list_projects(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_list_projects(Self::auth(&ctx)).await)
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

    #[tool(
        description = "What registering (or promoting) a project requires, before you try: per classification tier the required evidence fields, the self-service budget ceiling, and the default budget; plus the registration policy (manager/group required?) and the valid data classes. Static operator config — call it once up front."
    )]
    async fn registration_requirements(
        &self,
        Parameters(a): Parameters<RegistrationRequirementsArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_registration_requirements(a))
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
        description = "List the MCP catalog — MCP servers other people have published and shared in this org. Each entry returns name, summary, structured install spec, tags, owner (contact), and tier (company-approved vs user-submitted). Use it to find an MCP server to install."
    )]
    async fn mcp_catalog_list(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_mcp_catalog_list().await)
    }

    #[tool(
        description = "Fetch one MCP catalog entry by id (see mcp_catalog_list) — full README plus the structured install spec to wire it into your client."
    )]
    async fn mcp_catalog_get(
        &self,
        Parameters(a): Parameters<McpCatalogGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_mcp_catalog_get(&a.id).await)
    }

    #[tool(
        description = "Publish an MCP server to the catalog so others can discover and install it (or update one you own by passing its id). install = { transport: stdio|remote, command, args, env, url }. Requires a user token (asg_pat_…) — the entry is owned by you as the contact point; a project key cannot publish. Your submission is listed as user-submitted until an admin promotes it to company-approved."
    )]
    async fn mcp_catalog_publish(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<McpCatalogPublishArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (email, role) = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => {
                return deny(
                    "publishing to the MCP catalog requires a user token (asg_pat_…); a project key has no owner identity".into(),
                )
            }
        };
        wrap(self.do_mcp_catalog_publish(&email, &role, a).await)
    }

    #[tool(
        description = "Change the lifecycle of an MCP catalog entry you own: disabled (temporarily hide), archived (retire — kept for audit, prunable), or active (restore). Requires a user token; owner or admin only."
    )]
    async fn mcp_catalog_set_state(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<McpCatalogStateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (email, role) = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => {
                return deny(
                    "changing a catalog entry's state requires a user token (asg_pat_…)".into(),
                )
            }
        };
        wrap(self.do_mcp_catalog_set_state(&email, &role, a).await)
    }

    #[tool(
        description = "List the Skills catalog — agent skills (a SKILL.md plus optional bundled scripts/config) people have published and shared in this org. Each entry returns name, summary, runtime, portability, tags, owner (contact), and tier (company-approved vs user-submitted). Use it to find a skill to install."
    )]
    async fn skills_catalog_list(&self) -> Result<CallToolResult, McpError> {
        wrap(self.do_skills_catalog_list().await)
    }

    #[tool(
        description = "Fetch one Skills catalog entry by id (see skills_catalog_list) — metadata, parsed SKILL.md frontmatter, and a portability verdict. Use skills_catalog_export to download the bundle, optionally translated to another runtime."
    )]
    async fn skills_catalog_get(
        &self,
        Parameters(a): Parameters<SkillsCatalogGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_skills_catalog_get(&a.id).await)
    }

    #[tool(
        description = "Publish an agent skill to the catalog so others can discover and install it (or update one you own by passing its id). bundle is the skill's file tree [{ path, content_b64 }] and must include a SKILL.md; name/summary default to the SKILL.md frontmatter. Requires a user token (asg_pat_…) — a project key has no owner identity. Your submission is listed as user-submitted until an admin promotes it to company-approved."
    )]
    async fn skills_catalog_publish(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<SkillsCatalogPublishArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (email, role) = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => {
                return deny(
                    "publishing to the Skills catalog requires a user token (asg_pat_…); a project key has no owner identity".into(),
                )
            }
        };
        wrap(self.do_skills_catalog_publish(&email, &role, a).await)
    }

    #[tool(
        description = "Change the lifecycle of a skill you own: disabled (temporarily hide), archived (retire — kept for audit, prunable), or active (restore). Requires a user token; owner or admin only."
    )]
    async fn skills_catalog_set_state(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<SkillsCatalogStateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (email, role) = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => return deny("changing a skill's state requires a user token (asg_pat_…)".into()),
        };
        wrap(self.do_skills_catalog_set_state(&email, &role, a).await)
    }

    #[tool(
        description = "Download a skill's bundle, translated to a target runtime (claude-code or codex; defaults to the skill's own). Returns the rendered file tree [{ path, content_b64 }] plus a loss report of anything the translation degraded. Read-only."
    )]
    async fn skills_catalog_export(
        &self,
        Parameters(a): Parameters<SkillsCatalogExportArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_skills_catalog_export(a).await)
    }

    #[tool(
        description = "Install a skill: returns the destination directory and the skill's files (translated for the runtime) as decoded text, so you write each file to <dir>/<path>. dest = claude-code | codex | cursor (default the skill's own runtime); cursor uses the Claude Code rendering. Read-only — it returns the files, you write them."
    )]
    async fn skills_catalog_install(
        &self,
        Parameters(a): Parameters<SkillsCatalogInstallArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.do_skills_catalog_install(a).await)
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
        description = "Request an infrastructure resource for a registered project (e.g. s3-bucket, dynamodb-table, random-secret). Self-service types provision immediately; review-tier types await approval (and deploy automatically once approved). Fast resources return a `provisioned` record; slow ones (RDS/ALB/ECS) return a `provisioning` record — poll get_resource with its id until state is `provisioned` or `failed`. Re-requesting the same resource type+name with a changed spec is an in-place update (the resource is re-applied, keeping its id and identity-bearing outputs); an identical re-request is a no-op."
    )]
    async fn request_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(mut a): Parameters<RequestResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        if a.requester.is_none() {
            a.requester = Self::requester_from_auth(&ctx);
        }
        wrap(self.do_request_resource(&pid, a).await)
    }

    #[tool(
        description = "Link pre-existing (brownfield) infrastructure to a project for cost attribution WITHOUT Frontkeep managing it: records a `linked` external resource whose declared cost source and monthly estimate flow into the project's cost rollup. Frontkeep never touches the infrastructure itself — tag the real cloud resources `project=<id>` so the source can attribute actuals. Unlink (record-only) with deprovision_resource. On a user token pass project_id; on a project key omit it."
    )]
    async fn link_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<LinkResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_link_resource(&pid, a).await)
    }

    #[tool(
        description = "Roll a provisioned container service (e.g. an ecs-service) onto a new image. Swaps only the image in the resource's spec — env, secrets, grants, and the listener cert are preserved — and re-applies in place, so ECS registers a new task-definition revision and rolls with circuit-breaker rollback. Use after pushing the new tag to ECR (see ecr-credential). Returns the `provisioning` record; poll get_resource until `provisioned` or `failed`. Re-deploying the same image is a no-op."
    )]
    async fn deploy_image(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(mut a): Parameters<DeployImageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        if a.requester.is_none() {
            a.requester = Self::requester_from_auth(&ctx);
        }
        wrap(self.do_deploy_image(&pid, a).await)
    }

    #[tool(
        description = "Grant a consumer resource (e.g. an ecs-service) access to a target resource in the same project (e.g. an s3-bucket or dynamodb-table). Defaults to read+write. Your own project's resources are self-service — no approval. Pass ids from list_resources."
    )]
    async fn request_grant(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(mut a): Parameters<RequestGrantArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id.clone()).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        if a.requester.is_none() {
            a.requester = Self::requester_from_auth(&ctx);
        }
        wrap(self.do_request_grant(&pid, a).await)
    }

    #[tool(
        description = "List a project's provisioned resources (id, type, name, state, outputs). Optionally filter by state. Use the ids here for request_grant and deprovision_resource."
    )]
    async fn list_resources(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ListResourcesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pid = match self.resolve_project(&ctx, a.project_id).await {
            Ok(p) => p,
            Err(e) => return deny(e),
        };
        wrap(self.do_list_resources(&pid, a.state.as_deref()).await)
    }

    #[tool(
        description = "List resource/budget requests awaiting human approval (state=requested) that you may see — for projects you own or manage (admins/finance see all). Requests land here only when they exceed the project's self-service cost ceiling or hit a force-review service; routine resources auto-provision. Pass project_id to narrow to one project. Clear them with approve_request / deny_request."
    )]
    async fn list_pending_approvals(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ListPendingApprovalsArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(
            self.do_list_pending_approvals(Self::auth(&ctx), a.project_id)
                .await,
        )
    }

    #[tool(
        description = "Approve a pending request (see list_pending_approvals); for a provisioning request the apply is enqueued immediately. Requires a user token (asg_pat_…): the project's manager or an admin may approve — the project owner cannot self-approve their own over-budget request, and a project key cannot approve at all. Returns the post-approval request (Fulfilled if it completed inline, else Approved while the apply runs in the background)."
    )]
    async fn approve_request(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ApprovalDecisionArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(
            self.do_decide_request(Self::auth(&ctx), &a.request_id, a.reason.as_deref(), true)
                .await,
        )
    }

    #[tool(
        description = "Deny a pending request (see list_pending_approvals). Requires a user token (asg_pat_…): the project's manager or an admin only. Records the reason and moves the request to rejected."
    )]
    async fn deny_request(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<ApprovalDecisionArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(
            self.do_decide_request(Self::auth(&ctx), &a.request_id, a.reason.as_deref(), false)
                .await,
        )
    }

    #[tool(
        description = "Fetch one provisioned resource by id — poll this to follow an async provision or teardown to its terminal state. Returns state (provisioning → provisioned/failed, or destroying → destroyed/destroy_failed), outputs, and error. Use after request_resource returns a `provisioning` record (slow services like RDS/ALB/ECS provision in the background)."
    )]
    async fn get_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<GetResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Same scoping as deprovision: the resource id is not a cross-project
        // handle — it must belong to a project the caller is authorized for.
        let rec = match self.provision.repo().get(&a.resource_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return deny(format!("resource {} not found", a.resource_id)),
            Err(e) => return deny(e.to_string()),
        };
        match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => {
                if let Err(e) = self.authorize_user(&email, &role, &rec.project_id).await {
                    return deny(e);
                }
            }
            Some(McpAuth::Project { project_id }) => {
                if rec.project_id != project_id {
                    return deny(format!("resource {} not found", a.resource_id));
                }
            }
            None => {
                if let Some(scope) = &self.default_project {
                    if &rec.project_id != scope {
                        return deny(format!("resource {} not found", a.resource_id));
                    }
                }
            }
        }
        wrap(Ok(serde_json::to_string(&rec).unwrap_or_default()))
    }

    #[tool(
        description = "Read the captured connector run-log for a resource: the full terraform plan+apply output (or exec/HTTP output) of every attempt, success and failure, timestamped — for audit and debugging a provision. Requires a user token whose role holds ViewAudit (admin/finance); the output can carry provider secrets, so it is encrypted at rest and admin-gated."
    )]
    async fn resource_runs(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<GetResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Audit surface: a user token with ViewAudit (admin/finance) only, then
        // the usual per-project authority check on the resolved record.
        let (email, role) = match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => (email, role),
            _ => return deny(
                "reading a resource run-log requires a user token (asg_pat_…) with audit access"
                    .into(),
            ),
        };
        if !Role::parse(&role).can(asgard_identity::Capability::ViewAudit) {
            return deny(format!(
                "role '{role}' is not permitted to read resource run-logs (requires ViewAudit)"
            ));
        }
        let rec = match self.provision.repo().get(&a.resource_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return deny(format!("resource {} not found", a.resource_id)),
            Err(e) => return deny(e.to_string()),
        };
        if let Err(e) = self.authorize_user(&email, &role, &rec.project_id).await {
            return deny(e);
        }
        wrap(
            self.provision
                .resource_runs(&a.resource_id)
                .await
                .map(|runs| serde_json::to_string(&runs).unwrap_or_default())
                .map_err(|e| e.to_string()),
        )
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
        description = "Request a one-step classification promotion (e.g. poc → light-operational). A clean Light target auto-approves. When a deep code-review reviewer is enabled it reads your repository in the background, so the request may come back as 'reviewing' — poll with promotion_status (or re-fetch the request) until it resolves. If the review finds fixable problems the request is returned to you as 'flagged' with `review_findings` — fix the evidence/repo and call this again to re-run (it supersedes the prior attempt), or escalate_promotion to forward it to a human. Clean Wide/Critical targets need a human by tier. See registration_requirements for the evidence fields each tier demands. Returns the workflow request."
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
        description = "Forward a flagged promotion to a human reviewer instead of fixing-and-retrying. Pass the flagged request_id from request_promotion. Use when you can't (or choose not to) resolve the review findings yourself."
    )]
    async fn escalate_promotion(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<EscalatePromotionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let req = match self.workflow.get(&a.request_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return deny(format!("request {} not found", a.request_id)),
            Err(e) => return deny(e.to_string()),
        };
        let project_id = req
            .subject
            .strip_prefix("project:")
            .unwrap_or_default()
            .to_string();
        match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => {
                if let Err(e) = self.authorize_user(&email, &role, &project_id).await {
                    return deny(e);
                }
                wrap(
                    self.do_escalate_promotion(&a.request_id, &format!("user:{email}"))
                        .await,
                )
            }
            Some(McpAuth::Project { project_id: pid }) => {
                if pid != project_id {
                    return deny("cross-project access denied".into());
                }
                wrap(
                    self.do_escalate_promotion(&a.request_id, &format!("project:{pid}"))
                        .await,
                )
            }
            None => wrap(
                self.do_escalate_promotion(&a.request_id, DEFAULT_REQUESTER)
                    .await,
            ),
        }
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
        description = "Manually retry a stuck resource now, bypassing the auto-retry backoff. Re-arms a failed/destroy_failed resource, or unsticks one stranded in provisioning/destroying behind a crashed worker (only if its claim is stale — a live in-flight apply is never disturbed). A no-op for any other state. Use the resource id from request_resource or list_resources."
    )]
    async fn retry_resource(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(a): Parameters<GetResourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Same scoping as deprovision: the resource must belong to a project the
        // caller is authorized for; the resource id is not a cross-project handle.
        let rec = match self.provision.repo().get(&a.resource_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return deny(format!("resource {} not found", a.resource_id)),
            Err(e) => return deny(e.to_string()),
        };
        match Self::auth(&ctx) {
            Some(McpAuth::User { email, role }) => {
                if let Err(e) = self.authorize_user(&email, &role, &rec.project_id).await {
                    return deny(e);
                }
            }
            Some(McpAuth::Project { project_id }) => {
                if rec.project_id != project_id {
                    return deny(format!("resource {} not found", a.resource_id));
                }
            }
            None => {
                if let Some(scope) = &self.default_project {
                    if &rec.project_id != scope {
                        return deny(format!("resource {} not found", a.resource_id));
                    }
                }
            }
        }
        wrap(
            self.provision
                .retry_resource(&a.resource_id)
                .await
                .map(|rec| serde_json::to_string(&rec).unwrap_or_default())
                .map_err(|e| e.to_string()),
        )
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
        description = "Set up a repo on Frontkeep in one call: returns the agent-seed plan with every file's body inlined (AGENTS.md + the .agent/ coding and security standards for the repo's languages/work). Write each file to its path, then register_project. One shot — no seed_plan/seed_get loop."
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
        description = "Set up the current repo on Frontkeep: pull the AGENTS.md starting point + coding/security standards, then register the project."
    )]
    async fn bootstrap_prompt(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "Set this repository up on Frontkeep now. Do it, don't describe it:\n\
             1. Call the `bootstrap` tool.\n\
             2. For every NEW file it returns, write the `body` verbatim to its `path` \
             (create directories as needed) — actually create the files on disk; do \
             not paraphrase or just summarize what the seed contains. If a file \
             already exists (e.g. this repo has an AGENTS.md or CLAUDE.md), merge \
             instead of overwriting: keep the repo's guidance and add the Frontkeep \
             sections it lacks.\n\
             3. Call `register_project` to register this project — ask me for the \
             owner, budget, and data classification if you don't already have them.\n\
             Start with step 1 now.",
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
        info.server_info = Implementation::new("frontkeep", env!("CARGO_PKG_VERSION"));
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

/// The placeholder PAT the Getting-Started snippets show before a token is minted;
/// a request carrying it verbatim is the classic "forgot to swap the token" setup
/// mistake, worth a dedicated hint rather than a generic "invalid token".
const PLACEHOLDER_PAT: &str = "asg_pat_your_user_token";

/// Best-effort `scheme://host` for the running deployment, derived from request
/// headers, so first-run auth errors can point the operator at a real Get-Started
/// page instead of a generic instruction.
fn self_origin(req: &Request) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<your-asgard-host>");
    let scheme = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| {
            if host.starts_with("localhost") || host.starts_with("127.") {
                "http"
            } else {
                "https"
            }
        });
    format!("{scheme}://{host}")
}

async fn mcp_auth(State(st): State<McpAuthState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);
    let origin = self_origin(&req);
    // An unset $FRONTKEEP_PAT expands to nothing in the client's `claude mcp add`, so
    // the stored header is `Bearer ` and the token arrives empty — the single most
    // common first-run failure. Name it instead of treating it as a bad key.
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                format!(
                    "no bearer token on the request. If you added the MCP server with \
                     `$FRONTKEEP_PAT`, it was unset when the command ran (the value is baked in \
                     at add-time). Mint a PAT at {origin} → Get Started and put it in the \
                     Authorization header."
                ),
            )
                .into_response();
        }
    };
    if token == PLACEHOLDER_PAT {
        return (
            StatusCode::UNAUTHORIZED,
            format!(
                "the bearer token is still the placeholder `{PLACEHOLDER_PAT}`. Replace it with \
                 a real PAT minted at {origin} → Get Started."
            ),
        )
            .into_response();
    }
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
            Err(_) => (
                StatusCode::UNAUTHORIZED,
                format!(
                    "invalid or revoked user token. Mint a fresh PAT at {origin} → Get Started."
                ),
            )
                .into_response(),
        };
    }
    match st.gateway_repo.verify_key(&token).await {
        Ok(Some(project_id)) => {
            req.extensions_mut().insert(McpAuth::Project { project_id });
            next.run(req).await
        }
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            format!(
                "invalid or revoked credential. Use a project key, or your user PAT \
                 (`{PAT_PREFIX}…`) minted at {origin} → Get Started."
            ),
        )
            .into_response(),
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
        let mut provision = ProvisionService::new(ProvisionRepo::new(db));
        provision.set_workflow(workflow.clone());
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
                    provisional: false,
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
        assert_eq!(info.server_info.name, "frontkeep");
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_some());
    }

    #[tokio::test]
    async fn every_tool_input_schema_is_client_valid() {
        let s = server(None).await;
        for tool in s.tool_router.list_all() {
            let props = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object());
            let Some(props) = props else { continue };
            for (field, schema) in props {
                assert!(
                    schema.is_object(),
                    "tool `{}` field `{}` renders as a non-object JSON Schema ({schema}); \
                     strict MCP clients reject this. Annotate the field with a concrete schema.",
                    tool.name,
                    field,
                );
            }
        }
    }

    #[tokio::test]
    async fn registration_requirements_exposes_evidence_and_ceilings() {
        let s = server(None).await;
        let out = s
            .do_registration_requirements(RegistrationRequirementsArgs {
                classification: None,
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tiers = v["classifications"].as_array().unwrap();
        assert_eq!(tiers.len(), 4);
        assert_eq!(tiers[0]["classification"], "poc");
        assert!(tiers[0]["required_evidence"].as_array().unwrap().is_empty());
        assert_eq!(tiers[0]["auto_approve_ceiling_usd"], 500.0);
        assert_eq!(tiers[0]["default_budget_usd"], 250.0);
        assert!(tiers[1]["required_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "repo_or_source_url"));
        assert!(v["registration"]["require_manager"].is_boolean());
        assert!(v["data_classes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "internal"));

        assert!(s
            .do_registration_requirements(RegistrationRequirementsArgs {
                classification: Some("bogus".into()),
            })
            .is_err());
        let one = s
            .do_registration_requirements(RegistrationRequirementsArgs {
                classification: Some("light-operational".into()),
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&one).unwrap();
        assert_eq!(v["classifications"].as_array().unwrap().len(), 1);
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
