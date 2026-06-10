//! LiteLLM connector: mints a per-project virtual key on a LiteLLM proxy through
//! the normal governed `request_resource` flow. Each project gets its own
//! budgeted, project-tagged key, calls LiteLLM directly, and Frontkeep pulls the
//! key's spend back via the `litellm` cost source. The key value is a secret
//! output (routed to the secret store); the `key_alias` stays plaintext so the
//! cost source can read spend back without unsealing anything.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plan, ProvisionError, ProvisionRequest, Provisioned, Provisioner};

pub struct LiteLlmConnector {
    base_url: String,
    master_key: String,
    client: reqwest::Client,
}

impl LiteLlmConnector {
    pub fn new(base_url: impl Into<String>, master_key: impl Into<String>) -> Self {
        LiteLlmConnector {
            base_url: base_url.into(),
            master_key: master_key.into(),
            client: reqwest::Client::new(),
        }
    }
}

/// The `/key/generate` request body for a project's virtual key. `max_budget` and
/// `models` are pulled from the request spec when present; the alias and metadata
/// always carry the project id so the cost source can read spend back per project.
fn build_generate_body(req: &ProvisionRequest) -> Value {
    let project_id = &req.ctx.project_id;
    let alias = format!("asgard-{}-{}", project_id, req.name);
    let mut body = json!({
        "key_alias": alias,
        "metadata": { "project_id": project_id, "managed_by": "asgard" },
        "budget_duration": "30d",
    });
    let map = body.as_object_mut().unwrap();
    if let Some(budget) = req.spec.get("max_budget_usd").and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }) {
        map.insert("max_budget".into(), json!(budget));
    }
    if let Some(models) = req.spec.get("models").and_then(|v| v.as_array()) {
        let models: Vec<&str> = models.iter().filter_map(|m| m.as_str()).collect();
        if !models.is_empty() {
            map.insert("models".into(), json!(models));
        }
    }
    body
}

/// Extract the minted key from a `/key/generate` response. LiteLLM returns
/// `{"key": "sk-…", "key_name": …, …}`.
fn parse_generate_response(payload: &Value) -> Result<String, ProvisionError> {
    payload
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ProvisionError::Backend("litellm /key/generate returned no key".into()))
}

#[async_trait]
impl Provisioner for LiteLlmConnector {
    fn name(&self) -> &str {
        "litellm"
    }
    fn dry_run(&self) -> bool {
        false
    }
    fn supports(&self, resource_type: &str) -> bool {
        resource_type == "litellm-key"
    }

    async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError> {
        Ok(Plan {
            summary: format!(
                "mint a litellm virtual key for project {}",
                req.ctx.project_id
            ),
            tags: req.ctx.tags(),
            estimated_monthly_usd: req.estimated_monthly_usd,
        })
    }

    async fn apply(
        &self,
        req: &ProvisionRequest,
        _plan: &Plan,
    ) -> Result<Provisioned, ProvisionError> {
        let alias = format!("asgard-{}-{}", req.ctx.project_id, req.name);
        let body = build_generate_body(req);
        let url = format!("{}/key/generate", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProvisionError::Backend(format!("litellm key/generate: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let detail = resp.text().await.unwrap_or_default();
            return Err(ProvisionError::Backend(format!(
                "litellm key/generate http {status}: {}",
                detail.trim()
            )));
        }
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| ProvisionError::Backend(format!("litellm key/generate body: {e}")))?;
        let key = parse_generate_response(&payload)?;
        Ok(Provisioned {
            outputs: json!({
                "api_key": key,
                "key_alias": alias,
                "litellm_base_url": self.base_url,
            }),
            resource_ids: vec![alias],
            sensitive_keys: req.secret_outputs.clone(),
        })
    }

    async fn destroy(
        &self,
        _req: &ProvisionRequest,
        outputs: &Value,
    ) -> Result<(), ProvisionError> {
        let Some(alias) = outputs.get("key_alias").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let url = format!("{}/key/delete", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .json(&json!({ "key_aliases": [alias] }))
            .send()
            .await
            .map_err(|e| ProvisionError::Backend(format!("litellm key/delete: {e}")))?;
        if !resp.status().is_success() {
            return Err(ProvisionError::Backend(format!(
                "litellm key/delete http {}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceContext;

    fn ctx() -> ResourceContext {
        ResourceContext {
            project_id: "proj-2026-0001".into(),
            owner: "o".into(),
            manager: "m".into(),
            group: "g".into(),
            cost_center: "cc".into(),
            classification: "poc".into(),
            environment: "dev".into(),
            cloud: "stub".into(),
            account: "local".into(),
        }
    }

    fn req(spec: Value) -> ProvisionRequest {
        ProvisionRequest {
            resource_type: "litellm-key".into(),
            name: "default".into(),
            ctx: ctx(),
            spec,
            config: json!({}),
            estimated_monthly_usd: 0.0,
            secret_outputs: vec!["api_key".into()],
            resource_id: None,
        }
    }

    #[test]
    fn body_carries_alias_metadata_and_budget() {
        let b = build_generate_body(&req(json!({ "max_budget_usd": 25.0 })));
        assert_eq!(b["key_alias"], "asgard-proj-2026-0001-default");
        assert_eq!(b["metadata"]["project_id"], "proj-2026-0001");
        assert_eq!(b["metadata"]["managed_by"], "asgard");
        assert_eq!(b["budget_duration"], "30d");
        assert_eq!(b["max_budget"], 25.0);
    }

    #[test]
    fn budget_accepts_string_and_models_optional() {
        let b = build_generate_body(&req(
            json!({ "max_budget_usd": "10", "models": ["gpt-5.1"] }),
        ));
        assert_eq!(b["max_budget"], 10.0);
        assert_eq!(b["models"], json!(["gpt-5.1"]));
        // No budget/models → keys absent, not null.
        let b2 = build_generate_body(&req(json!({})));
        assert!(b2.get("max_budget").is_none());
        assert!(b2.get("models").is_none());
    }

    #[test]
    fn parses_minted_key() {
        let v = json!({ "key": "sk-abc123", "key_name": "asgard-proj-default" });
        assert_eq!(parse_generate_response(&v).unwrap(), "sk-abc123");
        assert!(parse_generate_response(&json!({ "error": "nope" })).is_err());
    }

    #[test]
    fn supports_only_litellm_key() {
        let c = LiteLlmConnector::new("http://x", "sk-master");
        assert!(c.supports("litellm-key"));
        assert!(!c.supports("s3-bucket"));
    }
}
