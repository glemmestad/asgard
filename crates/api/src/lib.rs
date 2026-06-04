//! REST + GraphQL HTTP surface, wiring catalog, gateway, workflow, eval, policy,
//! and identity. An `x-asgard-trace-id` is ensured on every response (brief §4).

pub mod error;
pub mod graphql;

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use asgard_catalog::{CatalogRepo, Entity, ListFilter, SchemaRegistry};
use asgard_gateway::{ChatMessage, ChatRequest, Gateway, TRACE_HEADER};
use asgard_identity::oidc::OidcConfig;
use asgard_identity::{IdentityService, OidcRoleConfig};
use asgard_provision::{ProvisionService, RollupDim};
use asgard_registry::{CostDim, ProjectRegistry, RegisterInput};
use asgard_storage::audit::{self, AuditQuery};
use asgard_storage::Db;
use asgard_workflow::{NewRequest, RequestFilter, State as WfState, WorkflowEngine};

use crate::error::ApiError;
use crate::graphql::{build_schema, AsgardSchema};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub catalog: CatalogRepo,
    pub schemas: SchemaRegistry,
    pub gateway: Arc<Gateway>,
    pub workflow: Arc<WorkflowEngine>,
    pub registry: ProjectRegistry,
    pub provision: ProvisionService,
    pub identity: IdentityService,
    pub gql: AsgardSchema,
    /// Display name for this deployment, shown as the wordmark/title in the UI.
    /// Defaults to `Asgard`; operators rebrand via `ASGARD_SYSTEM_NAME`.
    pub system_name: String,
    /// A platform-owned gateway key the dashboard's cost Q&A uses when the caller
    /// supplies none, so the human-first dashboard works without pasting a key.
    /// Spend is attributed to the internal system project like any other call.
    pub system_cost_key: Option<String>,
    /// The model the cost Q&A uses when the request doesn't name one. Real model
    /// (e.g. `model:default/gpt-5`) when a provider key is configured, else mock.
    pub cost_qa_model: String,
    /// OIDC login configuration (enterprise upgrade). `None` = local-users only;
    /// the `/api/auth/oidc/*` routes 404 when absent.
    pub oidc: Option<OidcConfig>,
    /// How OIDC sign-ins map to roles (admin emails + group-claim sync). `None`
    /// when nothing is configured: OIDC users default to `member` and roles are
    /// managed manually. When [`OidcRoleConfig::authoritative`] holds, the IdP
    /// owns OIDC roles and the role API rejects manual changes to them.
    pub oidc_roles: Option<OidcRoleConfig>,
    /// When true, local username/password sign-in is fully disabled (no
    /// exceptions, including the bootstrap admin) and the UI drops the password
    /// form. Refused at startup unless OIDC is configured, to avoid lockout.
    pub disable_local_login: bool,
    /// Dev escape hatch (rung 3): when true, session enforcement on human/admin
    /// routes is disabled. Off by default; only honored on a loopback bind (the
    /// binary refuses to set it otherwise). For throwaway local hacking only.
    pub dev_insecure: bool,
    /// Force `Secure` on auth cookies regardless of the request scheme. Enterprises
    /// that terminate TLS everywhere set this so a cookie can never be issued
    /// non-`Secure` (vs. the default, which marks `Secure` only when TLS is
    /// detected). "HTTPS is required," not "HTTPS if detected."
    pub force_https: bool,
    /// Best-effort brute-force throttle for local login (per-source, in-memory,
    /// per-replica). Argon2 already makes each attempt expensive; this caps
    /// sustained guessing.
    pub login_throttle: LoginThrottle,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        catalog: CatalogRepo,
        schemas: SchemaRegistry,
        gateway: Arc<Gateway>,
        workflow: Arc<WorkflowEngine>,
        registry: ProjectRegistry,
        provision: ProvisionService,
        identity: IdentityService,
    ) -> Self {
        let gql = build_schema(catalog.clone(), db.clone());
        AppState {
            db,
            catalog,
            schemas,
            gateway,
            workflow,
            registry,
            provision,
            identity,
            gql,
            system_name: "Asgard".to_string(),
            system_cost_key: None,
            cost_qa_model: "model:default/mock".to_string(),
            oidc: None,
            oidc_roles: None,
            disable_local_login: false,
            dev_insecure: false,
            force_https: false,
            login_throttle: LoginThrottle::default(),
        }
    }

    pub fn with_system_name(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        let name = name.trim();
        if !name.is_empty() {
            self.system_name = name.to_string();
        }
        self
    }

    pub fn with_system_cost_key(mut self, key: Option<String>) -> Self {
        self.system_cost_key = key;
        self
    }

    pub fn with_cost_qa_model(mut self, model: impl Into<String>) -> Self {
        self.cost_qa_model = model.into();
        self
    }

    pub fn with_oidc(mut self, oidc: Option<OidcConfig>) -> Self {
        self.oidc = oidc;
        self
    }

    pub fn with_oidc_roles(mut self, roles: Option<OidcRoleConfig>) -> Self {
        self.oidc_roles = roles;
        self
    }

    pub fn with_disable_local_login(mut self, disable: bool) -> Self {
        self.disable_local_login = disable;
        self
    }

    pub fn with_dev_insecure(mut self, dev_insecure: bool) -> Self {
        self.dev_insecure = dev_insecure;
        self
    }

    pub fn with_force_https(mut self, force_https: bool) -> Self {
        self.force_https = force_https;
        self
    }
}

pub fn router(state: AppState) -> Router {
    // Human/admin + dashboard-data surface: requires a valid human session
    // (rung 1/2). Agent surfaces are gated by project virtual keys instead and
    // live in the open group below; `/mcp` is mounted separately and key-gated.
    let protected = Router::new()
        .route("/api/catalog/entities", get(list_entities))
        .route(
            "/api/catalog/entities/{kind}/{namespace}/{name}",
            get(get_entity),
        )
        .route(
            "/api/catalog/entities/{kind}/{namespace}/{name}/catalog-info",
            get(get_catalog_info),
        )
        .route("/api/projects", get(list_projects).post(register_project))
        .route("/api/projects/{id}", get(get_project).patch(update_project))
        .route("/api/projects/{id}/keys", post(mint_key))
        .route("/api/projects/{id}/kill", post(kill_project))
        .route("/api/projects/{id}/unkill", post(unkill_project))
        .route(
            "/api/projects/{id}/promotion",
            get(promotion_checklist).post(request_promotion),
        )
        .route("/api/projects/{id}/demote", post(demote_project))
        .route("/api/projects/{id}/extend", post(extend_review))
        .route(
            "/api/projects/{id}/decommission",
            post(decommission_project),
        )
        .route("/api/projects/{id}/usage", get(project_usage))
        .route(
            "/api/projects/{id}/resources",
            get(list_resources).post(request_resource),
        )
        .route(
            "/api/projects/{id}/resources/{rid}",
            delete(deprovision_resource),
        )
        .route("/api/projects/{id}/secrets", get(list_project_secrets))
        .route(
            "/api/projects/{id}/secrets/{name}/rotate",
            post(rotate_project_secret),
        )
        .route("/api/services", get(list_services))
        .route("/api/services/{id}", get(get_service))
        // The cost surface is open to any signed-in user but each handler scopes
        // its results to what the caller is entitled to see: Admin/Finance get
        // everything, everyone else only the projects they own or manage.
        .route("/api/cost", get(cost_report))
        .route("/api/cost/series", get(cost_series))
        .route("/api/cost/by", get(cost_by))
        .route("/api/cost/forecast", get(cost_forecast))
        .route("/api/cost/anomalies", get(cost_anomalies))
        .route("/api/cost/tree", get(cost_tree))
        .route("/api/cost/movers", get(cost_movers))
        .route("/api/cost/tagged", get(cost_tagged))
        .route("/api/cost/project/{id}/series", get(cost_project_series))
        .route("/api/cost/ask", post(cost_ask))
        .route("/api/cost/rollup", post(cost_rollup))
        .route("/api/governance/metrics", get(governance_metrics))
        .route("/api/registry/sweep", post(registry_sweep))
        .route("/api/groups", get(list_groups))
        .route("/api/standards", get(list_standards).post(put_standard))
        .route("/api/standards/{id}", get(get_standard))
        .route("/api/standards/{id}/history", get(standard_history))
        .route("/api/seed", get(list_seed))
        .route("/api/seed/{id}", get(get_seed))
        .route("/api/guidance", get(list_guidance).post(put_guidance))
        .route("/api/guidance/{slug}", get(get_guidance))
        .route("/api/guidance/{slug}/approve", post(approve_guidance))
        .route("/api/guidance/{slug}/history", get(guidance_history))
        .route("/api/recipes", get(list_recipes).post(put_recipe))
        .route("/api/recipes/{slug}", get(get_recipe))
        .route("/api/recipes/{slug}/approve", post(approve_recipe))
        .route("/api/recipes/{slug}/history", get(recipe_history))
        .route(
            "/api/mcp-servers",
            get(list_mcp_servers).post(create_mcp_server),
        )
        .route(
            "/api/mcp-servers/{id}",
            get(get_mcp_server)
                .put(update_mcp_server)
                .delete(delete_mcp_server),
        )
        .route("/api/mcp-servers/{id}/approve", post(approve_mcp_server))
        .route(
            "/api/mcp-servers/{id}/unapprove",
            post(unapprove_mcp_server),
        )
        .route("/api/mcp-servers/{id}/disable", post(disable_mcp_server))
        .route("/api/mcp-servers/{id}/enable", post(enable_mcp_server))
        .route("/api/mcp-servers/{id}/archive", post(archive_mcp_server))
        .route(
            "/api/mcp-servers/{id}/unarchive",
            post(unarchive_mcp_server),
        )
        .route("/api/mcp-servers/{id}/history", get(mcp_server_history))
        .route("/api/requests", get(list_requests).post(submit_request))
        .route("/api/requests/{id}/approve", post(approve_request))
        .route("/api/requests/{id}/reject", post(reject_request))
        .route("/api/requests/{id}/fulfill", post(fulfill_request))
        .route("/api/audit", get(list_audit))
        .route("/api/users", get(list_users_route).post(create_user_route))
        .route("/api/users/{id}/role", post(set_user_role_route))
        .route("/api/users/{id}/active", post(set_user_active_route))
        .route("/api/auth/tokens", get(list_tokens).post(create_token))
        .route("/api/auth/tokens/{id}", delete(revoke_token))
        .route("/graphql", get(graphql_playground).post(graphql_handler))
        .route_layer(from_fn_with_state(state.clone(), require_session));

    // Open surface: health, the auth handshake itself, and the project-key-gated
    // gateway. These must be reachable without a human session.
    let open = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/gateway/chat", post(gateway_chat))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/config", get(auth_config))
        .route("/api/auth/oidc/login", get(oidc_login))
        .route("/api/auth/oidc/callback", get(oidc_callback));

    // No CORS layer: the dashboard is served same-origin from this binary, and the
    // API/MCP consumers are not browsers. Cross-origin browser access is therefore
    // denied by default (no `Access-Control-Allow-*`) rather than allowed wide.
    protected
        .merge(open)
        .layer(axum::middleware::from_fn(trace_mw))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

const SESSION_COOKIE: &str = "asgard_session";
const OIDC_STATE_COOKIE: &str = "oidc_state";

/// Read a session token from either the `Authorization: Bearer` header (API
/// clients) or the `asgard_session` cookie (browser, set by login/OIDC).
fn session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(b) = bearer(headers) {
        return Some(b);
    }
    cookie(headers, SESSION_COOKIE)
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
}

/// Session-enforcement middleware for the human/admin surface. The dev escape
/// hatch (loopback-only, set by the binary) short-circuits it; otherwise a valid
/// session (Bearer token or `asgard_session` cookie) is required.
async fn require_session(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if st.dev_insecure {
        return next.run(req).await;
    }
    let token = session_token(req.headers());
    match token {
        Some(t) if st.identity.validate_session(&t).await.is_ok() => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            "authentication required: sign in at / or present a session token",
        )
            .into_response(),
    }
}

/// The synthetic admin returned on the loopback dev hatch — never persisted.
fn dev_admin_user() -> asgard_identity::User {
    asgard_identity::User {
        id: "dev-insecure".into(),
        username: "admin".into(),
        email: None,
        display_name: Some("Dev (insecure)".into()),
        provider: "dev-insecure".into(),
        is_admin: true,
        role: "admin".into(),
        active: true,
        created_at: String::new(),
    }
}

/// Resolve the acting user from the request — a valid session, or the synthetic
/// admin under the loopback dev hatch. 401 otherwise.
async fn current_user(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<asgard_identity::User, ApiError> {
    if let Some(token) = session_token(headers) {
        if let Ok(user) = st.identity.validate_session(&token).await {
            return Ok(user);
        }
    }
    if st.dev_insecure {
        return Ok(dev_admin_user());
    }
    Err(ApiError::Unauthorized("authentication required".into()))
}

/// Resolve the acting user and require a capability, else 403.
async fn require_cap(
    st: &AppState,
    headers: &HeaderMap,
    cap: asgard_identity::Capability,
) -> Result<asgard_identity::User, ApiError> {
    let user = current_user(st, headers).await?;
    if user.can(cap) {
        Ok(user)
    } else {
        Err(ApiError::Forbidden(format!(
            "role '{}' is not permitted to perform this action",
            user.role
        )))
    }
}

/// Resolve the acting user and require they have authority over a *specific*
/// project — its owner or manager, or an admin/finance see-all role. The same
/// relationship rule used for cost/project *visibility*, now enforced on
/// mutations so a signed-in user can't act on a project by id alone. 403 otherwise.
async fn require_project_authority(
    st: &AppState,
    headers: &HeaderMap,
    project_id: &str,
) -> Result<asgard_identity::User, ApiError> {
    let user = current_user(st, headers).await?;
    let see_all = scope_for(&user).is_none();
    let email = user.email.clone().unwrap_or_default();
    if st
        .registry
        .is_authority(project_id, &email, see_all)
        .await?
    {
        Ok(user)
    } else {
        Err(ApiError::Forbidden(format!(
            "not authorized for project {project_id} (you must own or manage it)"
        )))
    }
}

/// The cost/projects visibility scope for a user: `None` means see-everything
/// (Admin/Finance, via the ViewAllCost capability); otherwise the caller's email,
/// which scopes reads to the projects they own or manage. A user with no email
/// who isn't see-all scopes to `""`, matching nothing — fail closed.
fn scope_for(user: &asgard_identity::User) -> Option<String> {
    if user.can(asgard_identity::Capability::ViewAllCost) {
        None
    } else {
        Some(user.email.clone().unwrap_or_default())
    }
}

pub async fn serve(state: AppState, bind: &str) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("asgard api listening on {bind}");
    axum::serve(listener, router(state)).await
}

async fn trace_mw(req: Request, next: Next) -> Response {
    let incoming = req.headers().get(TRACE_HEADER).cloned();
    let mut resp = next.run(req).await;
    let key = HeaderName::from_static(TRACE_HEADER);
    if !resp.headers().contains_key(&key) {
        let hv = incoming.unwrap_or_else(|| {
            HeaderValue::from_str(&format!("tr_{}", asgard_storage::new_uid()))
                .unwrap_or(HeaderValue::from_static("tr_unknown"))
        });
        resp.headers_mut().insert(key, hv);
    }
    resp
}

/// Liveness: the process is up. Static — does not touch dependencies.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the process can serve — confirms the database is reachable. Use
/// this for orchestrator readiness probes; `healthz` for liveness.
async fn readyz(State(st): State<AppState>) -> Response {
    match st.db.ping().await {
        Ok(_) => (StatusCode::OK, "ready").into_response(),
        Err(e) => {
            tracing::warn!("readiness check failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

#[derive(Deserialize)]
struct EntityQuery {
    kind: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
}

async fn list_entities(
    State(st): State<AppState>,
    Query(q): Query<EntityQuery>,
) -> Result<Json<Vec<Entity>>, ApiError> {
    let filter = ListFilter {
        kind: q.kind,
        query: q.q,
        limit: q.limit,
        ..Default::default()
    };
    Ok(Json(st.catalog.list(&filter).await?))
}

async fn get_entity(
    State(st): State<AppState>,
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<Entity>, ApiError> {
    st.catalog
        .get(&kind, &namespace, &name)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("{kind}:{namespace}/{name}")))
}

async fn get_catalog_info(
    State(st): State<AppState>,
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let e = st
        .catalog
        .get(&kind, &namespace, &name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("{kind}:{namespace}/{name}")))?;
    let yaml = emit_catalog_info(&e);
    Ok(([(header::CONTENT_TYPE, "application/yaml")], yaml).into_response())
}

#[derive(Deserialize)]
struct ChatBody {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    data_class: Option<String>,
}

async fn gateway_chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> Result<Response, ApiError> {
    let key = bearer(&headers)
        .ok_or_else(|| ApiError::Unauthorized("missing bearer virtual key".into()))?;
    let trace = headers
        .get(TRACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let req = ChatRequest {
        model: body.model,
        messages: body.messages,
        max_tokens: body.max_tokens,
        temperature: body.temperature,
        user: None,
    };
    let resp = st
        .gateway
        .complete(&key, req, trace, body.data_class)
        .await?;
    let trace_id = resp.trace_id.clone();
    let mut out = Json(resp).into_response();
    if let Ok(hv) = HeaderValue::from_str(&trace_id) {
        out.headers_mut()
            .insert(HeaderName::from_static(TRACE_HEADER), hv);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct MintBody {
    #[serde(default)]
    name: Option<String>,
}

async fn mint_key(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Only an authority over this project (owner/manager, or admin/finance) may
    // mint its key. Gate: the project must also be registered + active. Budget and
    // data class come from the registration record, not the mint request.
    require_project_authority(&st, &headers, &project_id).await?;
    st.registry.require_active(&project_id).await?;
    let minted = st
        .gateway
        .repo()
        .mint_key(&project_id, body.name.as_deref())
        .await?;
    Ok(Json(serde_json::json!({
        "key": minted.plaintext,
        "prefix": minted.prefix,
        "project_id": project_id,
    })))
}

async fn kill_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Only an authority over this project may flip its kill switch. Same gate as
    // minting/provisioning: it only applies to a registered, active project (a
    // phantom id must 404, not silently "succeed").
    require_project_authority(&st, &headers, &project_id).await?;
    st.registry.require_active(&project_id).await?;
    // Block the LLM key first (instant), then suspend billable resources.
    st.gateway.repo().set_killed(&project_id, true).await?;
    let summary = st.provision.suspend_project(&project_id).await?;
    let _ = audit::append(
        &st.db,
        &audit::AuditRecord::new("api", "project.kill")
            .entity(format!("project:{project_id}"))
            .outcome("killed")
            .data(serde_json::to_value(&summary).unwrap_or_default()),
    )
    .await;
    Ok(Json(serde_json::json!({
        "project_id": project_id,
        "killed": true,
        "suspended": summary.suspended,
        "skipped": summary.skipped,
        "failed": summary.failed,
    })))
}

async fn unkill_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    st.registry.require_active(&project_id).await?;
    st.gateway.repo().set_killed(&project_id, false).await?;
    let summary = st.provision.resume_project(&project_id).await?;
    let _ = audit::append(
        &st.db,
        &audit::AuditRecord::new("api", "project.unkill")
            .entity(format!("project:{project_id}"))
            .outcome("active")
            .data(serde_json::to_value(&summary).unwrap_or_default()),
    )
    .await;
    Ok(Json(serde_json::json!({
        "project_id": project_id,
        "killed": false,
        "resumed": summary.resumed,
        "failed": summary.failed,
    })))
}

async fn project_usage(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spent = st.gateway.repo().project_spend(&project_id).await?;
    Ok(Json(
        serde_json::json!({"project_id": project_id, "spent_usd": spent}),
    ))
}

async fn register_project(
    State(st): State<AppState>,
    Json(input): Json<RegisterInput>,
) -> Result<Json<asgard_registry::Registration>, ApiError> {
    let actor = format!(
        "user:default/{}",
        input.owner_email.split('@').next().unwrap_or("api")
    );
    Ok(Json(st.registry.register(input, &actor).await?))
}

async fn list_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<asgard_registry::Registration>>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let mut projects = st.registry.list().await?;
    // Same relationship rule as cost: a scoped caller sees only the projects they
    // own or manage; Admin/Finance (scope None) see all.
    if let Some(email) = scope {
        projects.retain(|p| p.owner == email || p.manager == email);
    }
    Ok(Json(projects))
}

async fn get_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<asgard_registry::Registration>, ApiError> {
    st.registry
        .get(&project_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("project {project_id}")))
}

async fn update_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(evidence): Json<asgard_registry::Evidence>,
) -> Result<Json<asgard_registry::Registration>, ApiError> {
    let user = require_project_authority(&st, &headers, &project_id).await?;
    let email = user.email.unwrap_or_default();
    let actor = format!("user:default/{}", email.split('@').next().unwrap_or("api"));
    Ok(Json(
        st.registry
            .update_evidence(&project_id, evidence, &actor)
            .await?,
    ))
}

#[derive(Deserialize)]
struct DecommissionBody {
    #[serde(default = "default_actor")]
    actor: String,
    #[serde(default)]
    reason: Option<String>,
}

fn default_actor() -> String {
    "user:default/api".to_string()
}

async fn decommission_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<DecommissionBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    let reason = b
        .reason
        .unwrap_or_else(|| "decommissioned via api".to_string());
    // Retire the project (gates via require_active), then tear down every
    // resource incl. data — irreversible.
    let registration = st
        .registry
        .decommission(&project_id, &b.actor, &reason)
        .await?;
    let teardown = st.provision.destroy_project_resources(&project_id).await?;
    Ok(Json(serde_json::json!({
        "registration": registration,
        "destroyed": teardown.destroyed,
        "failed": teardown.failed,
    })))
}

async fn promotion_checklist(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<asgard_registry::PromotionChecklist>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    Ok(Json(st.registry.promotion_checklist(&project_id).await?))
}

#[derive(Deserialize)]
struct PromotionBody {
    target: String,
}

async fn request_promotion(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<PromotionBody>,
) -> Result<Json<asgard_workflow::WorkflowRequest>, ApiError> {
    let user = require_project_authority(&st, &headers, &project_id).await?;
    let email = user.email.unwrap_or_default();
    let actor = format!("user:default/{}", email.split('@').next().unwrap_or("api"));
    Ok(Json(
        st.registry
            .request_promotion(&st.workflow, &project_id, &b.target, &actor)
            .await?,
    ))
}

#[derive(Deserialize)]
struct DemoteBody {
    target: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn demote_project(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<DemoteBody>,
) -> Result<Json<asgard_registry::Registration>, ApiError> {
    let user = require_project_authority(&st, &headers, &project_id).await?;
    let email = user.email.unwrap_or_default();
    let actor = format!("user:default/{}", email.split('@').next().unwrap_or("api"));
    let reason = b.reason.unwrap_or_default();
    Ok(Json(
        st.registry
            .demote(&project_id, &b.target, &actor, &reason)
            .await?,
    ))
}

async fn extend_review(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<asgard_registry::ExtendOutcome>, ApiError> {
    let user = require_project_authority(&st, &headers, &project_id).await?;
    let email = user.email.unwrap_or_default();
    let actor = format!("user:default/{}", email.split('@').next().unwrap_or("api"));
    Ok(Json(
        st.registry
            .extend_review(&st.workflow, &project_id, &actor)
            .await?,
    ))
}

#[derive(Deserialize)]
struct CostQuery {
    by: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

async fn cost_report(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CostQuery>,
) -> Result<Json<asgard_registry::CostReport>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let by = CostDim::parse(q.by.as_deref().unwrap_or("project")).ok_or_else(|| {
        ApiError::BadRequest(
            "by must be one of: project, owner, manager, group, classification, model, provider"
                .into(),
        )
    })?;
    Ok(Json(
        st.registry
            .cost_report(by, q.since.as_deref(), q.until.as_deref(), scope.as_deref())
            .await?,
    ))
}

// --- Cost rollup + dashboard (Phase 2/3) ---------------------------------
// All read-only over the denormalized cost_rollup rows; mirrored as MCP tools.

fn month_start(day: &str) -> String {
    format!("{}-01", &day[..7.min(day.len())])
}

#[derive(Deserialize)]
struct SeriesQuery {
    project: Option<String>,
    from: Option<String>,
    until: Option<String>,
}

async fn cost_series(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let project = q
        .project
        .ok_or_else(|| ApiError::BadRequest("project is required".into()))?;
    let today = asgard_provision::today();
    let from = q.from.unwrap_or_else(|| month_start(&today));
    let until = q.until.unwrap_or(today);
    let rows = st
        .provision
        .rollup_repo()
        .scoped(scope)
        .series(&project, &from, &until)
        .await?;
    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

async fn cost_project_series(
    State(st): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let today = asgard_provision::today();
    let from = q.from.unwrap_or_else(|| month_start(&today));
    let until = q.until.unwrap_or(today);
    let repo = st.provision.rollup_repo().scoped(scope);
    let rows = repo.series(&project, &from, &until).await?;
    let forecast = repo.latest_forecast(&project).await?;
    Ok(Json(
        serde_json::json!({ "series": rows, "forecast": forecast }),
    ))
}

#[derive(Deserialize)]
struct ByQuery {
    dim: Option<String>,
    from: Option<String>,
    until: Option<String>,
}

async fn cost_by(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ByQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let dim = RollupDim::parse(q.dim.as_deref().unwrap_or("project")).ok_or_else(|| {
        ApiError::BadRequest(
            "dim must be one of: project, owner, manager, group, cost_center, classification, service"
                .into(),
        )
    })?;
    let today = asgard_provision::today();
    let from = q.from.unwrap_or_else(|| month_start(&today));
    let until = q.until.unwrap_or(today);
    let rows = st
        .provision
        .rollup_repo()
        .scoped(scope)
        .by_dimension(dim, &from, &until)
        .await?;
    Ok(Json(
        serde_json::json!({ "by": dim.as_str(), "rows": rows }),
    ))
}

#[derive(Deserialize)]
struct ProjectQuery {
    project: Option<String>,
}

async fn cost_forecast(
    State(st): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let project = q
        .project
        .ok_or_else(|| ApiError::BadRequest("project is required".into()))?;
    let f = st.provision.rollup_repo().latest_forecast(&project).await?;
    Ok(Json(serde_json::to_value(f).unwrap_or_default()))
}

#[derive(Deserialize)]
struct AnomalyQuery {
    project: Option<String>,
    limit: Option<i64>,
}

async fn cost_anomalies(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AnomalyQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let rows = st
        .provision
        .rollup_repo()
        .scoped(scope)
        .anomalies(q.project.as_deref(), q.limit.unwrap_or(50))
        .await?;
    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

#[derive(Deserialize)]
struct AsOfQuery {
    as_of: Option<String>,
    top: Option<usize>,
}

async fn cost_tree(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let as_of = q.as_of.unwrap_or_else(asgard_provision::today);
    let tree = st
        .provision
        .cost_tree(&st.registry, &as_of, scope.as_deref())
        .await?;
    Ok(Json(serde_json::to_value(tree).unwrap_or_default()))
}

async fn cost_movers(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let as_of = q.as_of.unwrap_or_else(asgard_provision::today);
    let movers = st
        .provision
        .cost_movers(&as_of, q.top.unwrap_or(5), scope.as_deref())
        .await?;
    Ok(Json(serde_json::to_value(movers).unwrap_or_default()))
}

async fn cost_tagged(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    let as_of = q.as_of.unwrap_or_else(asgard_provision::today);
    let tagged = st.provision.cost_tagged(&as_of, scope.as_deref()).await?;
    Ok(Json(serde_json::to_value(tagged).unwrap_or_default()))
}

#[derive(Deserialize)]
struct AskBody {
    question: String,
    #[serde(default)]
    model: Option<String>,
}

async fn cost_ask(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<AskBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The AI answer is grounded in the whole rollup, so it's a see-everything
    // feature for now (Admin/Finance); scoped Q&A is a later refinement.
    require_cap(&st, &headers, asgard_identity::Capability::ViewAllCost).await?;
    let key = bearer(&headers)
        .or_else(|| st.system_cost_key.clone())
        .ok_or_else(|| ApiError::Unauthorized("missing bearer virtual key".into()))?;
    let model = b.model.unwrap_or_else(|| st.cost_qa_model.clone());
    let as_of = asgard_provision::today();
    let budgets = st
        .registry
        .list()
        .await?
        .into_iter()
        .map(|r| (r.project_id, r.budget_usd))
        .collect();
    let answer = st
        .provision
        .cost_qa(
            &st.gateway,
            &key,
            &model,
            None,
            &as_of,
            &b.question,
            budgets,
        )
        .await?;
    Ok(Json(serde_json::json!({ "answer": answer })))
}

#[derive(Deserialize)]
struct RollupBody {
    #[serde(default)]
    day: Option<String>,
}

/// Trigger one rollup pass (ops/e2e). The periodic task does this on a schedule
/// in `serve()`; this exposes the same idempotent routine on demand.
async fn cost_rollup(
    State(st): State<AppState>,
    Json(b): Json<RollupBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let day = b.day.unwrap_or_else(asgard_provision::today);
    let summary = st.provision.roll_up_costs(&st.registry, &day).await?;
    Ok(Json(serde_json::to_value(summary).unwrap_or_default()))
}

/// Portfolio governance metrics (WS4), scoped like the cost surface: Admin/Finance
/// see all projects, everyone else only the ones they own or manage.
async fn governance_metrics(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<asgard_registry::GovernanceMetrics>, ApiError> {
    let scope = scope_for(&current_user(&st, &headers).await?);
    Ok(Json(
        st.registry.governance_metrics(scope.as_deref()).await?,
    ))
}

/// Trigger one review-date sweep (ops/e2e): flag overdue reviews and lapsed
/// stack exceptions, auditing each. Idempotent; the periodic task in `serve()`
/// runs the same routine on a schedule.
async fn registry_sweep(
    State(st): State<AppState>,
) -> Result<Json<asgard_registry::SweepSummary>, ApiError> {
    Ok(Json(st.registry.sweep("system").await?))
}

async fn list_groups(State(st): State<AppState>) -> Json<serde_json::Value> {
    let groups: Vec<serde_json::Value> = st
        .registry
        .allowlist()
        .entries()
        .iter()
        .filter(|e| e.active)
        .map(|e| {
            serde_json::json!({"key": e.key, "display_name": e.display_name, "cost_center": e.cost_center})
        })
        .collect();
    Json(serde_json::json!({"open": st.registry.allowlist().is_open(), "groups": groups}))
}

#[derive(Deserialize)]
struct KnowledgeListQ {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    q: Option<String>,
}

async fn list_guidance(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(ql): Query<KnowledgeListQ>,
) -> Result<Json<Vec<asgard_registry::Guidance>>, ApiError> {
    // Admins see the pending-approval queue too; everyone else, published only.
    let include_pending = current_user(&st, &headers)
        .await
        .map(|u| u.can(asgard_identity::Capability::ManageUsers))
        .unwrap_or(false);
    Ok(Json(
        st.registry
            .guidance_list(include_pending, ql.category.as_deref(), ql.q.as_deref())
            .await?,
    ))
}

async fn approve_guidance(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    st.registry.guidance_approve(&slug).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "slug": slug, "status": "published" }),
    ))
}

async fn get_guidance(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<asgard_registry::Guidance>, ApiError> {
    st.registry
        .guidance_get(&slug)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("guidance {slug}")))
}

#[derive(Deserialize)]
struct GuidanceBody {
    #[serde(default)]
    slug: Option<String>,
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    category: String,
}

async fn put_guidance(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<GuidanceBody>,
) -> Result<Json<asgard_registry::Guidance>, ApiError> {
    let user = current_user(&st, &headers).await?;
    // An admin's own write publishes directly (they hold approval authority); a
    // non-admin submission is a draft until an admin approves it.
    let published = user.can(asgard_identity::Capability::ManageUsers);
    let g = st
        .registry
        .guidance_put(
            b.slug.as_deref(),
            &b.title,
            &b.summary,
            &b.body,
            &b.tags,
            &user.username,
            published,
            &b.category,
        )
        .await?;
    Ok(Json(g))
}

async fn guidance_history(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<asgard_registry::Version>>, ApiError> {
    Ok(Json(
        st.registry.knowledge_history("guidance", &slug).await?,
    ))
}

async fn list_recipes(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(ql): Query<KnowledgeListQ>,
) -> Result<Json<Vec<asgard_registry::Recipe>>, ApiError> {
    // Admins see the pending-approval queue too; everyone else, published only.
    let include_pending = current_user(&st, &headers)
        .await
        .map(|u| u.can(asgard_identity::Capability::ManageUsers))
        .unwrap_or(false);
    Ok(Json(
        st.registry
            .recipe_list(include_pending, ql.q.as_deref())
            .await?,
    ))
}

async fn get_recipe(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<asgard_registry::Recipe>, ApiError> {
    st.registry
        .recipe_get(&slug)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("recipe {slug}")))
}

#[derive(Deserialize)]
struct RecipeBody {
    #[serde(default)]
    slug: Option<String>,
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    spec: serde_json::Value,
    #[serde(default)]
    tags: Vec<String>,
}

async fn put_recipe(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<RecipeBody>,
) -> Result<Json<asgard_registry::Recipe>, ApiError> {
    let user = current_user(&st, &headers).await?;
    // An admin's own write publishes directly; a non-admin submission is a draft.
    let published = user.can(asgard_identity::Capability::ManageUsers);
    let r = st
        .registry
        .recipe_put(
            b.slug.as_deref(),
            &b.name,
            &b.summary,
            &b.body,
            &b.spec,
            &b.tags,
            &user.username,
            published,
        )
        .await?;
    Ok(Json(r))
}

async fn approve_recipe(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    st.registry.recipe_approve(&slug).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "slug": slug, "status": "published" }),
    ))
}

async fn recipe_history(
    State(st): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<asgard_registry::Version>>, ApiError> {
    Ok(Json(st.registry.knowledge_history("recipes", &slug).await?))
}

// ---- MCP catalog: user-publishable MCP servers (separate from provisioning) ----

#[derive(Deserialize)]
struct McpListQ {
    #[serde(default)]
    q: Option<String>,
    /// Trust tier: `community` | `approved`.
    #[serde(default)]
    status: Option<String>,
    /// Lifecycle view: `active` (default), `disabled`, `archived`, or `all`.
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct McpServerBody {
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    readme: String,
    #[serde(default)]
    install: serde_json::Value,
    #[serde(default)]
    repository: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    tags: Vec<String>,
}

impl From<McpServerBody> for asgard_registry::McpServerInput {
    fn from(b: McpServerBody) -> Self {
        asgard_registry::McpServerInput {
            name: b.name,
            summary: b.summary,
            readme: b.readme,
            install: b.install,
            repository: b.repository,
            homepage: b.homepage,
            version: b.version,
            tags: b.tags,
        }
    }
}

/// The contact identity recorded as an entry's owner: the caller's email, or
/// their username when no email is on the account (e.g. the dev-insecure admin).
fn owner_id(user: &asgard_identity::User) -> String {
    user.email.clone().unwrap_or_else(|| user.username.clone())
}

/// Owner-or-admin: the entry's owner, or a caller holding `ManageUsers`.
fn owns_or_admin(user: &asgard_identity::User, entry: &asgard_registry::McpServer) -> bool {
    user.can(asgard_identity::Capability::ManageUsers) || owner_id(user) == entry.owner
}

async fn list_mcp_servers(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(ql): Query<McpListQ>,
) -> Result<Json<Vec<asgard_registry::McpServer>>, ApiError> {
    let user = current_user(&st, &headers).await?;
    let state = ql.state.as_deref().unwrap_or("active");
    let mut list = st
        .registry
        .mcp_server_list(ql.q.as_deref(), ql.status.as_deref(), Some(state))
        .await?;
    // The active catalog is public; the disabled/archived management views are
    // scoped to the caller's own entries unless they can see everything.
    if state != "active" && !user.can(asgard_identity::Capability::ManageUsers) {
        let me = owner_id(&user);
        list.retain(|m| m.owner == me);
    }
    Ok(Json(list))
}

async fn create_mcp_server(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<McpServerBody>,
) -> Result<Json<asgard_registry::McpServer>, ApiError> {
    let user = current_user(&st, &headers).await?;
    // An admin's own publish lands as the company-approved tier; everyone else's
    // is listed immediately as user-submitted (community) with them as contact.
    let approved = user.can(asgard_identity::Capability::ManageUsers);
    let m = st
        .registry
        .mcp_server_create(&owner_id(&user), &b.into(), approved)
        .await?;
    Ok(Json(m))
}

async fn get_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<asgard_registry::McpServer>, ApiError> {
    st.registry
        .mcp_server_get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))
}

async fn update_mcp_server(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<McpServerBody>,
) -> Result<Json<asgard_registry::McpServer>, ApiError> {
    let user = current_user(&st, &headers).await?;
    let existing = st
        .registry
        .mcp_server_get(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if !owns_or_admin(&user, &existing) {
        return Err(ApiError::Forbidden(
            "only the owner or an admin can edit this catalog entry".into(),
        ));
    }
    let m = st
        .registry
        .mcp_server_update(&id, &b.into(), &owner_id(&user))
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    Ok(Json(m))
}

async fn delete_mcp_server(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = current_user(&st, &headers).await?;
    let existing = st
        .registry
        .mcp_server_get(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if !owns_or_admin(&user, &existing) {
        return Err(ApiError::Forbidden(
            "only the owner or an admin can delete this catalog entry".into(),
        ));
    }
    st.registry.mcp_server_delete(&id, &owner_id(&user)).await?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

async fn approve_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    st.registry
        .mcp_server_approve(&id, &owner_id(&user))
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "id": id, "status": "approved" }),
    ))
}

async fn unapprove_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    st.registry
        .mcp_server_unapprove(&id, &owner_id(&user))
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "id": id, "status": "community" }),
    ))
}

/// Shared lifecycle transition: owner-or-admin moves an entry to `state`, audited
/// under `action`.
async fn mcp_transition(
    st: &AppState,
    headers: &HeaderMap,
    id: &str,
    state: &str,
    action: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = current_user(st, headers).await?;
    let existing = st
        .registry
        .mcp_server_get(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if !owns_or_admin(&user, &existing) {
        return Err(ApiError::Forbidden(
            "only the owner or an admin can change this catalog entry's state".into(),
        ));
    }
    st.registry
        .mcp_server_set_state(id, state, action, &owner_id(&user))
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "id": id, "state": state }),
    ))
}

async fn disable_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    mcp_transition(&st, &headers, &id, "disabled", "disabled").await
}

async fn enable_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    mcp_transition(&st, &headers, &id, "active", "enabled").await
}

async fn archive_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    mcp_transition(&st, &headers, &id, "archived", "archived").await
}

async fn unarchive_mcp_server(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    mcp_transition(&st, &headers, &id, "active", "unarchived").await
}

async fn mcp_server_history(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<asgard_registry::Version>>, ApiError> {
    Ok(Json(
        st.registry.knowledge_history("mcp_server", &id).await?,
    ))
}

async fn list_standards(
    State(st): State<AppState>,
    Query(ql): Query<KnowledgeListQ>,
) -> Result<Json<Vec<asgard_registry::Standard>>, ApiError> {
    Ok(Json(st.registry.standard_list(ql.q.as_deref()).await?))
}

async fn get_standard(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<asgard_registry::Standard>, ApiError> {
    st.registry
        .standard_get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("standard {id}")))
}

#[derive(Deserialize)]
struct StandardBody {
    id: String,
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    body: String,
}

async fn put_standard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<StandardBody>,
) -> Result<Json<asgard_registry::Standard>, ApiError> {
    // Standards are normative: admin-only, always published, but versioned.
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    let user = current_user(&st, &headers).await?;
    let s = st
        .registry
        .standard_put(&b.id, &b.title, &b.summary, &b.body, &user.username)
        .await?;
    Ok(Json(s))
}

async fn standard_history(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<asgard_registry::Version>>, ApiError> {
    Ok(Json(st.registry.knowledge_history("standards", &id).await?))
}

// The agent-seed, surfaced read-only so a human can audit exactly what an agent
// is told (the same layered content `seed_plan`/`seed_get` serve over MCP).
async fn list_seed() -> Json<serde_json::Value> {
    let list: Vec<serde_json::Value> = asgard_catalog::seed::all()
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id, "title": m.title, "kind": m.kind.as_str(), "path": m.path,
                "tier": m.tier.as_str(), "summary": m.summary,
                "languages": m.languages, "keywords": m.keywords,
            })
        })
        .collect();
    Json(serde_json::json!(list))
}

async fn get_seed(Path(id): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    asgard_catalog::seed::get(&id)
        .map(|m| {
            Json(serde_json::json!({
                "id": m.id, "title": m.title, "kind": m.kind.as_str(),
                "path": m.path, "tier": m.tier.as_str(), "body": m.body,
            }))
        })
        .ok_or_else(|| ApiError::NotFound(format!("seed module {id}")))
}

#[derive(Deserialize)]
struct ResourceBody {
    resource_type: String,
    name: String,
    #[serde(default)]
    spec: serde_json::Value,
    #[serde(default = "default_actor")]
    requester: String,
}

async fn request_resource(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ResourceBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    let spec = if b.spec.is_null() {
        serde_json::json!({})
    } else {
        b.spec
    };
    let outcome = st
        .provision
        .request(
            &st.workflow,
            &st.registry,
            &project_id,
            &b.resource_type,
            &b.name,
            spec,
            &b.requester,
        )
        .await?;
    Ok(Json(serde_json::to_value(outcome).unwrap_or_default()))
}

async fn list_resources(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let resources = st.provision.repo().list_by_project(&project_id).await?;
    Ok(Json(serde_json::to_value(resources).unwrap_or_default()))
}

#[derive(Deserialize)]
struct DeprovisionQuery {
    #[serde(default = "default_actor")]
    actor: String,
}

/// Tear down a provisioned resource (routes to the manifest's connector
/// `destroy` and marks the record destroyed). The resource must belong to the
/// project in the path.
async fn deprovision_resource(
    State(st): State<AppState>,
    Path((project_id, rid)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<DeprovisionQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    let rec = st
        .provision
        .repo()
        .get(&rid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("resource {rid}")))?;
    if rec.project_id != project_id {
        return Err(ApiError::NotFound(format!("resource {rid}")));
    }
    let destroyed = st.provision.deprovision(&rid, &q.actor).await?;
    Ok(Json(serde_json::to_value(destroyed).unwrap_or_default()))
}

/// The service catalog (manifest-driven). Human/programmatic mirror of the
/// agent-first MCP `list_services` tool.
async fn list_services(State(st): State<AppState>) -> Json<serde_json::Value> {
    let services: Vec<serde_json::Value> = st
        .provision
        .catalog()
        .list()
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect();
    Json(serde_json::json!(services))
}

async fn get_service(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.provision
        .catalog()
        .get(&id)
        .map(|m| Json(serde_json::to_value(m).unwrap_or_default()))
        .ok_or_else(|| ApiError::NotFound(format!("service {id}")))
}

/// Secret metadata for a project (never values). Values are fetched only over
/// the project-credential-gated MCP `get_secret` tool.
async fn list_project_secrets(
    State(st): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let list = st.provision.list_secrets(&project_id).await?;
    Ok(Json(serde_json::to_value(list).unwrap_or_default()))
}

async fn rotate_project_secret(
    State(st): State<AppState>,
    Path((project_id, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(b): Json<ActionBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_authority(&st, &headers, &project_id).await?;
    let sref = st
        .provision
        .rotate_secret(&project_id, &name, &b.actor)
        .await?;
    Ok(Json(serde_json::to_value(sref).unwrap_or_default()))
}

#[derive(Deserialize)]
struct SubmitBody {
    kind: String,
    requester: String,
    subject: String,
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    sla_seconds: Option<i64>,
}

async fn submit_request(
    State(st): State<AppState>,
    Json(body): Json<SubmitBody>,
) -> Result<Json<asgard_workflow::WorkflowRequest>, ApiError> {
    let r = st
        .workflow
        .submit(NewRequest {
            kind: body.kind,
            requester: body.requester,
            subject: body.subject,
            payload: body.payload,
            sla_seconds: body.sla_seconds,
        })
        .await?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct ReqQuery {
    state: Option<String>,
    requester: Option<String>,
}

async fn list_requests(
    State(st): State<AppState>,
    Query(q): Query<ReqQuery>,
) -> Result<Json<Vec<asgard_workflow::WorkflowRequest>>, ApiError> {
    let filter = RequestFilter {
        state: q.state.as_deref().map(WfState::parse),
        requester: q.requester,
        limit: None,
    };
    Ok(Json(st.workflow.list(&filter).await?))
}

#[derive(Deserialize)]
struct ActionBody {
    actor: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn approve_request(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ActionBody>,
) -> Result<Json<asgard_workflow::WorkflowRequest>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ApproveRequests).await?;
    Ok(Json(
        st.workflow
            .approve(&id, &b.actor, b.reason.as_deref())
            .await?,
    ))
}

async fn reject_request(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ActionBody>,
) -> Result<Json<asgard_workflow::WorkflowRequest>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ApproveRequests).await?;
    Ok(Json(
        st.workflow
            .reject(&id, &b.actor, b.reason.as_deref())
            .await?,
    ))
}

async fn fulfill_request(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ActionBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Provisioning requests run the provisioner on fulfill; other kinds are a
    // plain state transition.
    let req = st
        .workflow
        .get(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("request {id}")))?;
    if req.kind.starts_with("provision:") {
        let outcome = st
            .provision
            .fulfill(&st.workflow, &st.registry, &id, &b.actor)
            .await?;
        Ok(Json(serde_json::to_value(outcome).unwrap_or_default()))
    } else if req.kind == "promotion" {
        let r = st
            .registry
            .fulfill_promotion(&st.workflow, &id, &b.actor)
            .await?;
        Ok(Json(serde_json::to_value(r).unwrap_or_default()))
    } else {
        let r = st.workflow.fulfill(&id, &b.actor).await?;
        Ok(Json(serde_json::to_value(r).unwrap_or_default()))
    }
}

#[derive(Deserialize)]
struct AuditQ {
    trace: Option<String>,
    entity: Option<String>,
    actor: Option<String>,
}

async fn list_audit(
    State(st): State<AppState>,
    Query(q): Query<AuditQ>,
) -> Result<Json<Vec<audit::AuditRecord>>, ApiError> {
    let query = AuditQuery {
        entity_ref: q.entity,
        trace_id: q.trace,
        actor: q.actor,
        limit: Some(500),
    };
    audit::query(&st.db, &query)
        .await
        .map(Json)
        .map_err(|e| ApiError::Internal(e.to_string()))
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

const SESSION_TTL_SECS: i64 = 8 * 3600;
const LOGIN_MAX_FAILS: u32 = 5;
const LOGIN_BLOCK: std::time::Duration = std::time::Duration::from_secs(300);

/// Per-source in-memory login throttle: after `LOGIN_MAX_FAILS` failures a source
/// is blocked for `LOGIN_BLOCK`. Best-effort (per-replica, not durable); the real
/// cost is Argon2 verification. Keyed by the client IP (X-Forwarded-For).
#[derive(Clone, Default)]
pub struct LoginThrottle {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<String, LoginAttempt>>>,
}

#[derive(Default)]
struct LoginAttempt {
    fails: u32,
    blocked_until: Option<std::time::Instant>,
}

impl LoginThrottle {
    /// `true` if the source may attempt a login right now.
    fn allowed(&self, key: &str) -> bool {
        let map = self.inner.lock().unwrap();
        match map.get(key).and_then(|a| a.blocked_until) {
            Some(until) => std::time::Instant::now() >= until,
            None => true,
        }
    }
    fn record_failure(&self, key: &str) {
        let mut map = self.inner.lock().unwrap();
        let a = map.entry(key.to_string()).or_default();
        a.fails += 1;
        if a.fails >= LOGIN_MAX_FAILS {
            a.blocked_until = Some(std::time::Instant::now() + LOGIN_BLOCK);
            a.fails = 0;
        }
    }
    fn record_success(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

/// Client IP for throttling: the first hop in `X-Forwarded-For` (set by the
/// ingress), else a shared bucket.
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether this request arrived over TLS. Adaptive by design: Asgard is an OSS
/// single-binary core that must run with no ancillary services, so it serves
/// plain http out of the box. `Secure` cookies are therefore set only when TLS is
/// actually present (a proxy sets `X-Forwarded-Proto: https`) — otherwise a plain
/// http first deployment could never establish a session. No TLS terminator is
/// ever *required* to get Asgard going.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

fn secure_suffix(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

/// Whether auth cookies should be `Secure`: forced on when the operator requires
/// HTTPS (`ASGARD_FORCE_HTTPS`), otherwise adaptive to the detected scheme.
fn cookie_secure(st: &AppState, headers: &HeaderMap) -> bool {
    st.force_https || is_https(headers)
}

/// `Set-Cookie` value for a session token. `HttpOnly` so JS can't read it (the
/// SPA confirms login via `/api/auth/me` instead); `SameSite=Lax` so the OIDC
/// redirect back to the app still carries it; `Secure` only under TLS.
fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{}; Max-Age={SESSION_TTL_SECS}",
        secure_suffix(secure)
    )
}

fn clear_cookie(name: &str, secure: bool) -> String {
    format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax{}; Max-Age=0",
        secure_suffix(secure)
    )
}

async fn login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<LoginBody>,
) -> Result<Response, ApiError> {
    if st.disable_local_login {
        return Err(ApiError::Forbidden(
            "local sign-in is disabled; use single sign-on".into(),
        ));
    }
    let ip = client_ip(&headers);
    if !st.login_throttle.allowed(&ip) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed sign-in attempts; try again later",
        )
            .into_response());
    }
    let user = match st
        .identity
        .authenticate_local(&b.username, &b.password)
        .await
    {
        Ok(u) => {
            st.login_throttle.record_success(&ip);
            u
        }
        Err(e) => {
            st.login_throttle.record_failure(&ip);
            return Err(e.into());
        }
    };
    let session = st
        .identity
        .create_session(&user.id, SESSION_TTL_SECS)
        .await
        .map_err(ApiError::from)?;
    let body = Json(serde_json::json!({ "token": session.token, "user": user }));
    Ok((
        [(
            header::SET_COOKIE,
            session_cookie(&session.token, cookie_secure(&st, &headers)),
        )],
        body,
    )
        .into_response())
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = session_token(&headers) {
        st.identity
            .revoke_session(&token)
            .await
            .map_err(ApiError::from)?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            clear_cookie(SESSION_COOKIE, cookie_secure(&st, &headers)),
        )],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response())
}

async fn me(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<asgard_identity::User>, ApiError> {
    // A valid session, or — on the loopback dev hatch — a synthetic admin, so the
    // embedded UI is browsable locally with no login. 401 otherwise.
    Ok(Json(current_user(&st, &headers).await?))
}

#[derive(Deserialize)]
struct CreateUserBody {
    username: String,
    password: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize)]
struct RoleBody {
    role: String,
}

#[derive(Deserialize)]
struct ActiveBody {
    active: bool,
}

async fn list_users_route(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<asgard_identity::User>>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    Ok(Json(st.identity.list_users().await?))
}

async fn create_user_route(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<CreateUserBody>,
) -> Result<Json<asgard_identity::User>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    let role = asgard_identity::Role::parse(b.role.as_deref().unwrap_or("member"));
    let u = st
        .identity
        .create_local_user(
            &b.username,
            &b.password,
            b.email.as_deref(),
            b.display_name.as_deref(),
            role,
        )
        .await?;
    Ok(Json(u))
}

async fn set_user_role_route(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<RoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    if st.oidc_roles.as_ref().is_some_and(|r| r.authoritative()) {
        let target = st.identity.get_user(&id).await?;
        if target.is_some_and(|u| u.provider == "oidc") {
            return Err(ApiError::Forbidden(
                "this user's role is managed by your identity provider".into(),
            ));
        }
    }
    st.identity
        .set_role(&id, asgard_identity::Role::parse(&b.role))
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn set_user_active_route(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ActiveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_cap(&st, &headers, asgard_identity::Capability::ManageUsers).await?;
    st.identity.set_active(&id, b.active).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// --- Personal access tokens: a user mints a long-lived, user-scoped credential
// for their agent (registers + manages every project they own/manage over MCP).

#[derive(Deserialize)]
struct CreateTokenBody {
    #[serde(default)]
    name: String,
    /// Optional lifetime in days; omitted = never expires (revoke to cut off).
    #[serde(default)]
    ttl_days: Option<i64>,
}

async fn create_token(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<CreateTokenBody>,
) -> Result<Json<asgard_identity::Pat>, ApiError> {
    let user = current_user(&st, &headers).await?;
    let name = if b.name.trim().is_empty() {
        "agent".to_string()
    } else {
        b.name
    };
    let ttl = b.ttl_days.filter(|d| *d > 0).map(|d| d * 86_400);
    Ok(Json(st.identity.create_pat(&user.id, &name, ttl).await?))
}

async fn list_tokens(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<asgard_identity::Pat>>, ApiError> {
    let user = current_user(&st, &headers).await?;
    Ok(Json(st.identity.list_pats(&user.id).await?))
}

async fn revoke_token(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = current_user(&st, &headers).await?;
    st.identity.revoke_pat(&user.id, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// Public: tells the login UI which sign-in methods are available (local is
/// always on; SSO appears only when OIDC is configured) and the operator's
/// registration policy (which fields the register form must require).
async fn auth_config(State(st): State<AppState>) -> Json<serde_json::Value> {
    let pol = st.registry.policy();
    Json(serde_json::json!({
        "local": !st.disable_local_login,
        "oidc": st.oidc.is_some(),
        "oidc_role_sync": st.oidc_roles.as_ref().is_some_and(|r| r.authoritative()),
        "system_name": st.system_name,
        "registration": {
            "require_manager": pol.require_manager,
            "require_group": pol.require_group,
        },
    }))
}

// --- OIDC login (rung 2): authorization-code flow against the operator's IdP --

async fn oidc_login(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(oidc) = st.oidc.as_ref() else {
        return (StatusCode::NOT_FOUND, "OIDC not configured").into_response();
    };
    let state = asgard_storage::new_uid();
    let nonce = asgard_storage::new_uid();
    let url = oidc.authorization_url(&state, &nonce);
    // Short-lived state cookie binds the callback to this request (CSRF guard).
    let cookie = format!(
        "{OIDC_STATE_COOKIE}={state}; Path=/; HttpOnly; SameSite=Lax{}; Max-Age=600",
        secure_suffix(cookie_secure(&st, &headers))
    );
    (
        StatusCode::FOUND,
        [(header::LOCATION, url), (header::SET_COOKIE, cookie)],
        "",
    )
        .into_response()
}

#[derive(Deserialize)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn oidc_callback(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<OidcCallbackQuery>,
) -> Response {
    let Some(oidc) = st.oidc.as_ref() else {
        return (StatusCode::NOT_FOUND, "OIDC not configured").into_response();
    };
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return (StatusCode::BAD_REQUEST, "missing code or state").into_response();
    };
    match cookie(&headers, OIDC_STATE_COOKIE) {
        Some(expected) if expected == state => {}
        _ => return (StatusCode::BAD_REQUEST, "state mismatch").into_response(),
    }
    let token = match oidc.exchange_code(&code).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("oidc code exchange failed: {e}");
            return (StatusCode::BAD_GATEWAY, "code exchange failed").into_response();
        }
    };
    let ui = match oidc.userinfo(&token.access_token).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("oidc userinfo failed: {e}");
            return (StatusCode::BAD_GATEWAY, "userinfo failed").into_response();
        }
    };
    let username = ui
        .preferred_username
        .clone()
        .or_else(|| ui.email.clone())
        .unwrap_or_else(|| ui.sub.clone());
    let (target_role, authoritative) = match st.oidc_roles.as_ref() {
        Some(rc) => {
            let groups = ui.groups(&rc.groups_claim);
            (
                rc.target_role(ui.email.as_deref(), &groups),
                rc.authoritative(),
            )
        }
        None => (None, false),
    };
    let user = match st
        .identity
        .upsert_oidc_user(
            &username,
            ui.email.as_deref(),
            ui.name.as_deref(),
            target_role,
            authoritative,
        )
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("oidc upsert failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "user provisioning failed",
            )
                .into_response();
        }
    };
    let session = match st.identity.create_session(&user.id, SESSION_TTL_SECS).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("oidc session create failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "session failed").into_response();
        }
    };
    // Set the session cookie, clear the one-shot state cookie, land on the app.
    let mut resp = (StatusCode::FOUND, [(header::LOCATION, "/")], "").into_response();
    let h = resp.headers_mut();
    let secure = cookie_secure(&st, &headers);
    if let Ok(v) = HeaderValue::from_str(&session_cookie(&session.token, secure)) {
        h.append(header::SET_COOKIE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&clear_cookie(OIDC_STATE_COOKIE, secure)) {
        h.append(header::SET_COOKIE, v);
    }
    resp
}

async fn graphql_handler(
    State(st): State<AppState>,
    req: async_graphql_axum::GraphQLRequest,
) -> async_graphql_axum::GraphQLResponse {
    st.gql.execute(req.into_inner()).await.into()
}

async fn graphql_playground() -> impl IntoResponse {
    Html(async_graphql::http::playground_source(
        async_graphql::http::GraphQLPlaygroundConfig::new("/graphql"),
    ))
}

/// Emit a Backstage-compatible `catalog-info.yaml` for an entity (one-way; brief §3.10).
pub fn emit_catalog_info(e: &Entity) -> String {
    let mut meta = serde_json::Map::new();
    meta.insert("name".into(), e.metadata.name.clone().into());
    meta.insert("namespace".into(), e.metadata.namespace.clone().into());
    if let Some(t) = &e.metadata.title {
        meta.insert("title".into(), t.clone().into());
    }
    if let Some(d) = &e.metadata.description {
        meta.insert("description".into(), d.clone().into());
    }
    if !e.metadata.tags.is_empty() {
        meta.insert(
            "tags".into(),
            serde_json::to_value(&e.metadata.tags).unwrap_or_default(),
        );
    }
    let doc = serde_json::json!({
        "apiVersion": "backstage.io/v1alpha1",
        "kind": e.kind,
        "metadata": serde_json::Value::Object(meta),
        "spec": e.spec,
    });
    serde_yaml::to_string(&doc).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asgard_catalog::{Manifest, Metadata, Origin};

    #[test]
    fn catalog_info_is_backstage_shaped_yaml() {
        let m = Manifest {
            api_version: Some("asgard.dev/v1".into()),
            kind: "Agent".into(),
            metadata: Metadata {
                name: "code-reviewer".into(),
                namespace: "default".into(),
                title: Some("Code Reviewer".into()),
                ..Default::default()
            },
            spec: serde_json::json!({"owner": "group:default/platform"}),
            relations: vec![],
        };
        let e = Entity::from_manifest(m, Origin::default());
        let yaml = emit_catalog_info(&e);
        assert!(yaml.contains("apiVersion: backstage.io/v1alpha1"));
        assert!(yaml.contains("kind: Agent"));
        assert!(yaml.contains("name: code-reviewer"));
    }
}
