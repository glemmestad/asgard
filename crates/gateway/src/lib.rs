//! The Frontkeep gateway: every model call routes through here. Per-project virtual
//! keys, budgets, kill switch, data-class×model policy, regex guardrails, full
//! audit with a propagated `x-frontkeep-trace-id`, and cost attribution (brief §4.2).
//! Non-optional, non-pluggable spine.

pub mod error;
pub mod guardrails;
pub mod keys;
pub mod provider;
pub mod registry;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use frontkeep_policy::{PolicyEngine, Request as PolicyRequest};
use frontkeep_storage::audit::{self, AuditRecord};
use serde::Serialize;

pub use error::GatewayError;
pub use guardrails::{Guardrails, Mode, Verdict};
pub use keys::{GatewayRepo, MintedKey, ProjectRuntime, UsageEvent};
pub use provider::{
    AnthropicProvider, ChatMessage, ChatRequest, ChatResponse, MockProvider, OpenAiProvider,
    Provider,
};
pub use registry::{ModelInfo, ModelRegistry};
pub use tools::{run_tool_loop, ToolDef, ToolExecutor, ToolLoopOutcome};

pub const TRACE_HEADER: &str = "x-frontkeep-trace-id";

#[derive(Debug, Clone, Serialize)]
pub struct GatewayResponse {
    pub content: String,
    pub trace_id: String,
    pub model: String,
    pub provider: String,
    pub cost_usd: f64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub guardrail_verdicts: Vec<Verdict>,
}

pub struct Gateway {
    repo: GatewayRepo,
    policy: Arc<dyn PolicyEngine>,
    guardrails: Guardrails,
    registry: ModelRegistry,
    providers: HashMap<String, Arc<dyn Provider>>,
    mode: Mode,
}

impl Gateway {
    pub fn new(
        repo: GatewayRepo,
        policy: Arc<dyn PolicyEngine>,
        registry: ModelRegistry,
        providers: HashMap<String, Arc<dyn Provider>>,
        mode: Mode,
    ) -> Self {
        Gateway {
            repo,
            policy,
            guardrails: Guardrails::builtin(),
            registry,
            providers,
            mode,
        }
    }

    pub fn repo(&self) -> &GatewayRepo {
        &self.repo
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    async fn audit(&self, rec: AuditRecord) {
        let _ = audit::append(self.repo.db(), &rec).await;
    }

    /// Route a chat completion through the full governance pipeline.
    pub async fn complete(
        &self,
        virtual_key: &str,
        mut req: ChatRequest,
        trace_id: Option<String>,
        data_class: Option<String>,
    ) -> Result<GatewayResponse, GatewayError> {
        let trace = trace_id.unwrap_or_else(|| format!("tr_{}", frontkeep_storage::new_uid()));

        // One round-trip resolves the key and the project runtime together.
        let rt = match self.repo.resolve_key(virtual_key).await? {
            Some(rt) => rt,
            None => {
                self.audit(
                    AuditRecord::new("gateway", "gateway.auth")
                        .trace(&trace)
                        .outcome("denied")
                        .reason("invalid or revoked key"),
                )
                .await;
                return Err(GatewayError::Unauthorized);
            }
        };
        let project_id = rt.project_id.clone();
        let principal = format!("project:{project_id}");

        let class = data_class.unwrap_or_else(|| rt.data_class.clone());

        if rt.lifecycle != "active" {
            self.audit(
                AuditRecord::new(&principal, "gateway.blocked")
                    .trace(&trace)
                    .entity(&req.model)
                    .outcome("denied")
                    .reason(format!("project lifecycle is {}", rt.lifecycle)),
            )
            .await;
            return Err(GatewayError::ProjectInactive);
        }

        if rt.killed {
            self.audit(
                AuditRecord::new(&principal, "gateway.blocked")
                    .trace(&trace)
                    .entity(&req.model)
                    .outcome("denied")
                    .reason("project kill switch engaged"),
            )
            .await;
            return Err(GatewayError::ProjectKilled);
        }

        let budget_exceeded = rt.budget_usd > 0.0 && rt.spent_usd >= rt.budget_usd;
        if budget_exceeded {
            self.audit(
                AuditRecord::new(&principal, "gateway.blocked")
                    .trace(&trace)
                    .entity(&req.model)
                    .outcome("denied")
                    .reason("budget exceeded"),
            )
            .await;
            return Err(GatewayError::BudgetExceeded);
        }

        let model = self
            .registry
            .resolve(&req.model)
            .ok_or_else(|| GatewayError::UnknownModel(req.model.clone()))?
            .clone();

        // Policy: data-class × model allowlist (plus kill/budget defense-in-depth).
        let decision = self
            .policy
            .is_authorized(&PolicyRequest::new(
                &principal,
                "invoke",
                &model.model_ref,
                serde_json::json!({
                    "data_class": class,
                    "model_data_classes": model.data_classes,
                    "project_killed": rt.killed,
                    "budget_exceeded": budget_exceeded,
                }),
            ))
            .await;
        if !decision.allowed() {
            let reason = decision.reasons.join("; ");
            self.audit(
                AuditRecord::new(&principal, "gateway.policy")
                    .trace(&trace)
                    .entity(&model.model_ref)
                    .outcome("denied")
                    .reason(&reason),
            )
            .await;
            return Err(GatewayError::PolicyDenied(reason));
        }

        // Guardrails on input.
        let input = self.guardrails.scan_input(self.mode, &mut req.messages);
        if let Some(reason) = input.blocked {
            self.audit(
                AuditRecord::new(&principal, "gateway.guardrail")
                    .trace(&trace)
                    .entity(&model.model_ref)
                    .outcome("blocked")
                    .reason(&reason)
                    .data(serde_json::to_value(&input.verdicts).unwrap_or_default()),
            )
            .await;
            return Err(GatewayError::GuardrailBlocked(reason));
        }
        let mut verdicts = input.verdicts;

        // Provider call.
        let provider = self
            .providers
            .get(&model.provider)
            .ok_or_else(|| GatewayError::NoProvider(model.provider.clone()))?;
        // Stamp downstream attribution so the provider's own logs/spend carry the
        // project (OpenAI `user` / Anthropic `metadata.user_id`). Authoritative —
        // never trust a caller-supplied value.
        req.user = Some(project_id.clone());
        let started = Instant::now();
        let resp = provider
            .chat(&model.route_model, &req)
            .await
            .map_err(|e| GatewayError::Provider(e.to_string()))?;
        let latency_ms = started.elapsed().as_millis() as u32;

        verdicts.extend(self.guardrails.scan_output(&resp.content));

        let cost = (resp.prompt_tokens as f64 / 1000.0) * model.cost_in
            + (resp.completion_tokens as f64 / 1000.0) * model.cost_out;

        let new_spent = self.repo.add_spend(&project_id, cost).await?;
        self.repo
            .record_usage(&UsageEvent {
                project_id: project_id.clone(),
                trace_id: Some(trace.clone()),
                model: model.model_ref.clone(),
                provider: model.provider.clone(),
                prompt_tokens: resp.prompt_tokens,
                completion_tokens: resp.completion_tokens,
                cost_usd: cost,
                latency_ms,
                owner: rt.owner.clone(),
                manager: rt.manager.clone(),
                cost_group: rt.cost_group.clone(),
                cost_center: rt.cost_center.clone(),
                classification: rt.classification.clone(),
            })
            .await?;

        // Soft cap: warn once when crossing 80% of budget.
        if rt.budget_usd > 0.0 {
            let soft = 0.8 * rt.budget_usd;
            if rt.spent_usd < soft && new_spent >= soft {
                self.audit(
                    AuditRecord::new(&principal, "gateway.budget_warn")
                        .trace(&trace)
                        .outcome("warn")
                        .reason(format!(
                            "crossed 80% of budget ({new_spent:.4}/{:.4})",
                            rt.budget_usd
                        )),
                )
                .await;
            }
        }

        self.audit(
            AuditRecord::new(&principal, "gateway.completion")
                .trace(&trace)
                .entity(&model.model_ref)
                .outcome("ok")
                .data(serde_json::json!({
                    "cost_usd": cost,
                    "prompt_tokens": resp.prompt_tokens,
                    "completion_tokens": resp.completion_tokens,
                    "latency_ms": latency_ms,
                    "provider": model.provider,
                })),
        )
        .await;

        Ok(GatewayResponse {
            content: resp.content,
            trace_id: trace,
            model: model.model_ref,
            provider: model.provider,
            cost_usd: cost,
            prompt_tokens: resp.prompt_tokens,
            completion_tokens: resp.completion_tokens,
            guardrail_verdicts: verdicts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontkeep_policy::CedarEngine;
    use frontkeep_storage::Db;

    async fn setup() -> (Gateway, String, String) {
        let path =
            std::env::temp_dir().join(format!("frontkeep-gw-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let repo = GatewayRepo::new(db);
        let project_id = "proj-2026-0001".to_string();
        repo.ensure_project(&project_id, 0.0, "internal")
            .await
            .unwrap();
        let key = repo.mint_key(&project_id, Some("default")).await.unwrap();

        let registry = ModelRegistry::from_models(vec![ModelInfo {
            model_ref: "model:default/mock-small".into(),
            provider: "mock".into(),
            route_model: "mock-small".into(),
            data_classes: vec!["public".into(), "internal".into()],
            cost_in: 1.0,
            cost_out: 2.0,
        }]);
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(MockProvider));
        let policy: Arc<dyn PolicyEngine> = Arc::new(CedarEngine::new().unwrap());
        let gw = Gateway::new(repo, policy, registry, providers, Mode::Enforce);
        (gw, project_id, key.plaintext)
    }

    fn req(text: &str) -> ChatRequest {
        ChatRequest {
            model: "model:default/mock-small".into(),
            messages: vec![ChatMessage::user(text)],
            max_tokens: None,
            temperature: None,
            user: None,
        }
    }

    /// Records the `user` (downstream attribution) of the last request it saw.
    struct CaptureProvider(std::sync::Arc<std::sync::Mutex<Option<String>>>);
    #[async_trait::async_trait]
    impl Provider for CaptureProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn chat(
            &self,
            route_model: &str,
            req: &ChatRequest,
        ) -> Result<provider::ChatResponse, provider::ProviderError> {
            *self.0.lock().unwrap() = req.user.clone();
            Ok(provider::ChatResponse {
                content: "ok".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
                model: route_model.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn forwards_project_as_downstream_attribution() {
        let path =
            std::env::temp_dir().join(format!("frontkeep-gw-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let repo = GatewayRepo::new(db);
        repo.ensure_project("proj-2026-0001", 0.0, "internal")
            .await
            .unwrap();
        let key = repo
            .mint_key("proj-2026-0001", None)
            .await
            .unwrap()
            .plaintext;
        let registry = ModelRegistry::from_models(vec![ModelInfo {
            model_ref: "model:default/mock-small".into(),
            provider: "mock".into(),
            route_model: "mock-small".into(),
            data_classes: vec!["internal".into()],
            cost_in: 1.0,
            cost_out: 1.0,
        }]);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(CaptureProvider(seen.clone())));
        let policy: Arc<dyn PolicyEngine> = Arc::new(CedarEngine::new().unwrap());
        let gw = Gateway::new(repo, policy, registry, providers, Mode::Enforce);
        gw.complete(&key, req("hi"), None, Some("internal".into()))
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            Some("proj-2026-0001".to_string()),
            "the gateway must forward the project id downstream, not the caller"
        );
    }

    #[tokio::test]
    async fn happy_path_attributes_cost() {
        let (gw, pid, key) = setup().await;
        let r = gw
            .complete(
                &key,
                req("hello there friend"),
                None,
                Some("internal".into()),
            )
            .await
            .unwrap();
        assert!(r.content.contains("hello there friend"));
        assert!(r.cost_usd > 0.0);
        assert!(!r.trace_id.is_empty());
        let spent = gw.repo().project_spend(&pid).await.unwrap();
        assert!((spent - r.cost_usd).abs() < 1e-9);
    }

    #[tokio::test]
    async fn unauthorized_key_rejected() {
        let (gw, _pid, _key) = setup().await;
        let e = gw.complete("fk_bogus", req("hi"), None, None).await;
        assert!(matches!(e, Err(GatewayError::Unauthorized)));
    }

    #[tokio::test]
    async fn wrong_data_class_denied_by_policy() {
        let (gw, _pid, key) = setup().await;
        let e = gw
            .complete(&key, req("hi"), None, Some("restricted".into()))
            .await;
        assert!(matches!(e, Err(GatewayError::PolicyDenied(_))), "got {e:?}");
    }

    #[tokio::test]
    async fn guardrail_blocks_leaked_secret() {
        let (gw, _pid, key) = setup().await;
        let e = gw
            .complete(
                &key,
                req("deploy with AKIAIOSFODNN7EXAMPLE now"),
                None,
                Some("internal".into()),
            )
            .await;
        assert!(
            matches!(e, Err(GatewayError::GuardrailBlocked(_))),
            "got {e:?}"
        );
    }

    #[tokio::test]
    async fn kill_switch_rejects_next_call() {
        let (gw, pid, key) = setup().await;
        gw.repo().set_killed(&pid, true).await.unwrap();
        let e = gw
            .complete(&key, req("hi"), None, Some("internal".into()))
            .await;
        assert!(matches!(e, Err(GatewayError::ProjectKilled)));
    }

    #[tokio::test]
    async fn budget_enforced_after_exhaustion() {
        let (gw, pid, key) = setup().await;
        gw.repo().set_budget(&pid, 0.001).await.unwrap(); // tiny
                                                          // First call succeeds and pushes spend over the cap.
        gw.complete(
            &key,
            req("hello there friend"),
            None,
            Some("internal".into()),
        )
        .await
        .unwrap();
        // Next call is blocked by the budget gate.
        let e = gw
            .complete(&key, req("again"), None, Some("internal".into()))
            .await;
        assert!(matches!(e, Err(GatewayError::BudgetExceeded)), "got {e:?}");
    }
}
