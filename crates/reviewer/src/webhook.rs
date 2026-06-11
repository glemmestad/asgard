//! External delegation (`kind: webhook`): POST the evidence + machine verdict to an
//! operator-configured reviewer (CodeRabbit/Greptile/in-house) and map its reply
//! back into the same escalate-only signals. Inert unless its `endpoint_env` is
//! set, so it never runs offline. Fails closed (a concern) on any transport error
//! — a governance gate must not pass silently because a reviewer was unreachable.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::manifest::ReviewerManifest;
use crate::{verdict_from_reply, ReviewError, ReviewRequest, ReviewVerdict, Reviewer};

pub struct WebhookReviewer {
    client: reqwest::Client,
}

impl Default for WebhookReviewer {
    fn default() -> Self {
        Self::new()
    }
}

impl WebhookReviewer {
    pub fn new() -> Self {
        WebhookReviewer {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Reviewer for WebhookReviewer {
    fn kind(&self) -> &str {
        "webhook"
    }

    async fn review(
        &self,
        m: &ReviewerManifest,
        req: &ReviewRequest,
    ) -> Result<ReviewVerdict, ReviewError> {
        let url = match m
            .endpoint_env
            .as_deref()
            .and_then(|e| std::env::var(e).ok())
        {
            Some(u) if !u.trim().is_empty() => u,
            // No endpoint configured → inert (the opt-in is "set the env var").
            _ => {
                return Ok(ReviewVerdict::abstain(
                    &m.id,
                    &m.kind,
                    "webhook endpoint not configured",
                ))
            }
        };

        let body = serde_json::json!({
            "project_id": req.project_id,
            "target": req.target,
            "data_class": req.data_class,
            "repo_url": req.repo_url,
            "evidence": req.evidence,
            "machine_verdict": req.machine_verdict,
        });
        let mut rb = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(m.timeout_secs));
        if let Some(key) = m.api_key_env.as_deref().and_then(|e| std::env::var(e).ok()) {
            rb = rb.bearer_auth(key);
        }

        let unreachable = |detail: String| {
            ReviewVerdict::concern(
                &m.id,
                &m.kind,
                vec![format!("external reviewer unreachable: {detail}")],
                format!("{}: external reviewer unreachable", m.id),
                0.0,
                String::new(),
                0.0,
            )
        };

        match rb.send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(v) => Ok(verdict_from_reply(&m.id, &m.kind, &v, String::new(), 0.0)),
                Err(e) => Ok(unreachable(format!("decode: {e}"))),
            },
            Err(e) => Ok(unreachable(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Disposition;
    use frontkeep_registry::EvidenceVerdict;

    fn req() -> ReviewRequest {
        ReviewRequest {
            project_id: "p".into(),
            target: "light-operational".into(),
            data_class: "internal".into(),
            repo_url: "https://git".into(),
            evidence: Default::default(),
            machine_verdict: EvidenceVerdict {
                target: "light-operational".into(),
                evidence_complete: true,
                missing: vec![],
                exception_signals: vec![],
                unverified_signals: vec![],
            },
        }
    }

    fn manifest(endpoint_env: Option<&str>) -> ReviewerManifest {
        ReviewerManifest {
            id: "external".into(),
            name: "External".into(),
            kind: "webhook".into(),
            enabled: true,
            dimensions: vec![],
            targets: vec![],
            model: "model:default/mock".into(),
            endpoint_env: endpoint_env.map(String::from),
            api_key_env: None,
            timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn inert_without_endpoint() {
        let r = WebhookReviewer::new();
        // No endpoint_env, and an env var that is unset.
        let v = r.review(&manifest(None), &req()).await.unwrap();
        assert_eq!(v.disposition, Disposition::Abstain);
        let v2 = r
            .review(&manifest(Some("FRONTKEEP_TEST_REVIEWER_URL_UNSET")), &req())
            .await
            .unwrap();
        assert_eq!(v2.disposition, Disposition::Abstain);
        assert!(v2.add_exception_signals.is_empty());
    }
}
