//! Gateway source: model/token spend already attributed to the project through
//! the LLM gateway's `usage_events`. The gateway is just one service in the hub;
//! this lets an inference service report its actual spend like any other.

use async_trait::async_trait;

use frontkeep_gateway::GatewayRepo;

use super::CostSource;
use crate::{CostWindow, ProvisionError, ServiceCost};

pub struct GatewaySource {
    repo: GatewayRepo,
}

impl GatewaySource {
    pub fn new(repo: GatewayRepo) -> Self {
        GatewaySource { repo }
    }
}

#[async_trait]
impl CostSource for GatewaySource {
    fn name(&self) -> &str {
        "gateway"
    }
    async fn cost(
        &self,
        project_id: &str,
        _window: &CostWindow,
    ) -> Result<ServiceCost, ProvisionError> {
        let spent = self
            .repo
            .project_spend(project_id)
            .await
            .map_err(|e| ProvisionError::Backend(format!("gateway spend: {e}")))?;
        Ok(ServiceCost {
            backend: "gateway".into(),
            actual_usd: Some(spent),
            source: "gateway-usage-events".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontkeep_gateway::UsageEvent;
    use frontkeep_storage::Db;

    #[tokio::test]
    async fn reports_attributed_model_spend() {
        let path =
            std::env::temp_dir().join(format!("frontkeep-gwc-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let repo = GatewayRepo::new(db);
        repo.record_usage(&UsageEvent {
            project_id: "proj-x".into(),
            trace_id: None,
            model: "model:default/mock".into(),
            provider: "mock".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_usd: 0.25,
            latency_ms: 1,
            owner: String::new(),
            manager: String::new(),
            cost_group: String::new(),
            cost_center: String::new(),
            classification: String::new(),
        })
        .await
        .unwrap();
        let src = GatewaySource::new(repo);
        let w = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };
        let c = src.cost("proj-x", &w).await.unwrap();
        assert_eq!(c.actual_usd, Some(0.25));
        // A project with no usage reports a real $0, not "unmeasured".
        assert_eq!(
            src.cost("proj-none", &w).await.unwrap().actual_usd,
            Some(0.0)
        );
    }
}
