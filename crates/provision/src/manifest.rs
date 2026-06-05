//! The manifest-driven service catalog: the source of truth for what a project
//! can provision. One YAML per service (`services/<id>/service.yaml`) declares
//! both how the service is provisioned (`provisioner.connector` + `config`) and
//! how its cost is attributed (`cost.source.type`). Adding a service is dropping
//! a manifest — no recompile. Defaults are embedded in the binary; an operator
//! overlay directory adds or overrides services at runtime.

use std::collections::BTreeMap;
use std::path::Path;

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

use crate::ProvisionError;

#[derive(RustEmbed)]
#[folder = "../../services"]
struct DefaultServices;

const CONNECTORS: &[&str] = &["terraform", "exec", "http", "mcp", "litellm", "stub"];
const INFERENCE_KINDS: &[&str] = &["openai", "anthropic", "openai-compatible"];
const COST_SOURCES: &[&str] = &[
    "none",
    "free",
    "flat",
    "gateway",
    "aws-cost-explorer",
    "gcp-billing",
    "azure-cost-management",
    "databricks-billing",
    "litellm",
    "exec",
];
const CLASSIFICATIONS: &[&str] = &[
    "poc",
    "light-operational",
    "wide-operational",
    "critical-path",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceManifest {
    pub id: String,
    pub name: String,
    pub category: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub classification_min: Option<String>,
    #[serde(default)]
    pub classification_max: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub auto_approvable: bool,
    #[serde(default)]
    pub required_fields: Vec<String>,
    /// Output keys whose values are secrets: the connector routes them to the
    /// secret store and records only a reference, never the value.
    #[serde(default)]
    pub secret_outputs: Vec<String>,
    pub provisioner: ProvisionerCfg,
    pub cost: CostCfg,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub documentation: Option<String>,
    /// Present on inference-backend modules (category `llm`): how the gateway
    /// reaches this LLM provider. The control plane (tokens/cost/audit) is the
    /// same regardless; this only declares the swappable upstream.
    #[serde(default)]
    pub inference: Option<InferenceCfg>,
    /// Cost/tier/approval keyed to a spec field (e.g. `instance_type`): small
    /// variants auto-approve on cost, big or sensitive ones gate to a tier or to
    /// human review — without a rules DSL.
    #[serde(default)]
    pub variants: Option<Variants>,
    /// Target side of an access grant: the access this resource can hand out, as
    /// `level → provider-native actions` (e.g. `read: [s3:GetObject, …]`). Only the
    /// resource knows its own verbs.
    #[serde(default)]
    pub access_levels: BTreeMap<String, Vec<String>>,
    /// Target side: how a grant against this resource is implemented. The mechanism
    /// is owned by the target service, so a new kind of target (EKS/IRSA, Athena/Lake
    /// Formation, …) ships its own module with no core change.
    #[serde(default)]
    pub grant: Option<GrantCfg>,
    /// Consumer side: the output key holding the identity a grant attaches access to
    /// (e.g. `task_role_arn`). Present on resources that can be granted access.
    #[serde(default)]
    pub principal_output: Option<String>,
    /// Consumer side: the kind of principal this resource provides (e.g. `iam-role`).
    /// Must match the target's `grant.principal_kind`.
    #[serde(default)]
    pub principal_kind: Option<String>,
    /// Latency hint only: when true, a provision request returns its `provisioning`
    /// record immediately instead of waiting the inline budget for completion (the
    /// apply runs in the background either way). Set on slow services (RDS, ALB,
    /// ECS). Never a correctness lever — the durability is the same regardless.
    #[serde(default)]
    pub long_running: bool,
}

/// How a grant against a target resource is bound. `module` is the connector
/// payload (for terraform, the TF module path); `principal_kind` is the identity
/// shape this mechanism accepts (e.g. `iam-role`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantCfg {
    pub module: String,
    pub principal_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variants {
    /// The spec field that selects the variant (e.g. `instance_type`).
    pub field: String,
    pub options: BTreeMap<String, Variant>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Variant {
    #[serde(default)]
    pub estimated_monthly_usd: Option<f64>,
    /// Minimum project classification for *this* variant (overrides the service
    /// floor). e.g. GPU sizes only at wide-operational and above.
    #[serde(default)]
    pub classification_min: Option<String>,
    /// Always route to human review regardless of cost (e.g. GPU instances).
    #[serde(default)]
    pub requires_approval: bool,
}

/// The effective cost/tier/approval for a request after resolving its variant.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub estimated_monthly_usd: f64,
    pub classification_min: Option<String>,
    pub force_review: bool,
}

/// Rank of a project classification (poc lowest). Unknown ⇒ lowest.
pub fn class_rank(c: &str) -> i32 {
    match c {
        "light-operational" => 1,
        "wide-operational" => 2,
        "critical-path" => 3,
        _ => 0,
    }
}

/// An inference backend a project's gateway token can route to. A standard
/// *enableable module* — the operator turns it on by supplying its credentials in
/// the environment, never by authoring a service definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceCfg {
    /// `openai` | `anthropic` | `openai-compatible` (LiteLLM, vLLM, Databricks, …).
    pub kind: String,
    /// Static endpoint, or the env var holding it (e.g. `LITELLM_BASE_URL`).
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub base_url_env: Option<String>,
    /// Override the request path for an `openai-compatible` upstream (default
    /// `/v1/chat/completions`). A `{model}` placeholder is substituted with the
    /// route model, so an endpoint-in-path upstream like Databricks Model Serving
    /// (`/serving-endpoints/{model}/invocations`) is a pure manifest — no code.
    #[serde(default)]
    pub chat_path: Option<String>,
    /// Env var holding the upstream master key (e.g. `OPENAI_MASTER_KEY`). The
    /// value never enters a manifest, token, or log — only its name does.
    pub api_key_env: String,
    #[serde(default)]
    pub models: Vec<InferenceModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceModel {
    #[serde(rename = "ref")]
    pub model_ref: String,
    pub route: String,
    #[serde(default)]
    pub data_classes: Vec<String>,
    #[serde(default)]
    pub cost_in: f64,
    #[serde(default)]
    pub cost_out: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionerCfg {
    pub connector: String,
    /// Connector-specific knobs (TF module, exec command, …). Each connector
    /// reads its own keys; the manifest carries them opaquely.
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostCfg {
    #[serde(default = "default_cost_model")]
    pub model: String,
    #[serde(default)]
    pub estimated_monthly_usd: f64,
    #[serde(default)]
    pub source: CostSourceCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSourceCfg {
    #[serde(rename = "type", default = "default_source_type")]
    pub source_type: String,
}

impl Default for CostSourceCfg {
    fn default() -> Self {
        CostSourceCfg {
            source_type: default_source_type(),
        }
    }
}

fn default_status() -> String {
    "live".to_string()
}
fn default_cost_model() -> String {
    "usage".to_string()
}
fn default_source_type() -> String {
    "none".to_string()
}

impl ServiceManifest {
    pub fn connector(&self) -> &str {
        &self.provisioner.connector
    }

    /// Resolve the effective cost/tier/approval for a request's spec. Without a
    /// `variants` block this is just the service-level estimate + floor. With one,
    /// the matched variant overrides; an *unrecognized* variant forces review (so
    /// an unknown size can't slip through auto-approval).
    pub fn resolve(&self, spec: &serde_json::Value) -> Resolved {
        let base = Resolved {
            estimated_monthly_usd: self.cost.estimated_monthly_usd,
            classification_min: self.classification_min.clone(),
            force_review: false,
        };
        let Some(v) = &self.variants else { return base };
        let key = spec.get(&v.field).and_then(|x| x.as_str());
        match key.and_then(|k| v.options.get(k)) {
            Some(o) => Resolved {
                estimated_monthly_usd: o
                    .estimated_monthly_usd
                    .unwrap_or(base.estimated_monthly_usd),
                classification_min: o.classification_min.clone().or(base.classification_min),
                force_review: o.requires_approval,
            },
            None => Resolved {
                force_review: true,
                ..base
            },
        }
    }

    /// Whether a project at `classification` may use this service (variant floor +
    /// service `classification_min`/`classification_max`). Returns the reason it is
    /// out of range, or `None` when allowed.
    pub fn tier_violation(&self, classification: &str, resolved: &Resolved) -> Option<String> {
        let rank = class_rank(classification);
        if let Some(min) = resolved
            .classification_min
            .as_deref()
            .or(self.classification_min.as_deref())
        {
            if rank < class_rank(min) {
                return Some(format!(
                    "service '{}' requires classification '{min}' or higher (project is '{classification}')",
                    self.id
                ));
            }
        }
        if let Some(max) = self.classification_max.as_deref() {
            if rank > class_rank(max) {
                return Some(format!(
                    "service '{}' is only available up to classification '{max}' (project is '{classification}')",
                    self.id
                ));
            }
        }
        None
    }

    /// The connector's config as an object (Null normalized to `{}`).
    pub fn connector_config(&self) -> serde_json::Value {
        match &self.provisioner.config {
            serde_json::Value::Null => serde_json::json!({}),
            v => v.clone(),
        }
    }

    fn validate(&self) -> Result<(), ProvisionError> {
        if self.id.trim().is_empty() {
            return Err(ProvisionError::InvalidSpec("manifest has empty id".into()));
        }
        if !CONNECTORS.contains(&self.provisioner.connector.as_str()) {
            return Err(ProvisionError::InvalidSpec(format!(
                "service '{}': unknown connector '{}'",
                self.id, self.provisioner.connector
            )));
        }
        if !COST_SOURCES.contains(&self.cost.source.source_type.as_str()) {
            return Err(ProvisionError::InvalidSpec(format!(
                "service '{}': unknown cost source '{}'",
                self.id, self.cost.source.source_type
            )));
        }
        if let Some(inf) = &self.inference {
            if !INFERENCE_KINDS.contains(&inf.kind.as_str()) {
                return Err(ProvisionError::InvalidSpec(format!(
                    "service '{}': unknown inference kind '{}'",
                    self.id, inf.kind
                )));
            }
            if inf.api_key_env.trim().is_empty() {
                return Err(ProvisionError::InvalidSpec(format!(
                    "service '{}': inference.api_key_env is required",
                    self.id
                )));
            }
        }
        for c in [&self.classification_min, &self.classification_max]
            .into_iter()
            .flatten()
        {
            if !CLASSIFICATIONS.contains(&c.as_str()) {
                return Err(ProvisionError::InvalidSpec(format!(
                    "service '{}': unknown classification '{}'",
                    self.id, c
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ServiceCatalog {
    services: BTreeMap<String, ServiceManifest>,
}

impl ServiceCatalog {
    /// The catalog embedded in the binary (the operator-curated defaults).
    pub fn embedded() -> Result<Self, ProvisionError> {
        let mut services = BTreeMap::new();
        for path in DefaultServices::iter() {
            if !path.ends_with("service.yaml") {
                continue;
            }
            let f = DefaultServices::get(&path).ok_or_else(|| {
                ProvisionError::InvalidSpec(format!("embedded manifest missing: {path}"))
            })?;
            let m = parse(&path, f.data.as_ref())?;
            services.insert(m.id.clone(), m);
        }
        Ok(ServiceCatalog { services })
    }

    /// Embedded defaults overlaid by an optional operator directory. Files named
    /// `service.yaml` anywhere under `overlay_dir` add or replace services by id.
    pub fn load(overlay_dir: Option<&Path>) -> Result<Self, ProvisionError> {
        let mut cat = Self::embedded()?;
        if let Some(dir) = overlay_dir {
            cat.overlay_dir(dir)?;
        }
        Ok(cat)
    }

    fn overlay_dir(&mut self, dir: &Path) -> Result<(), ProvisionError> {
        let mut files = Vec::new();
        collect_manifests(dir, &mut files)?;
        for path in files {
            let bytes = std::fs::read(&path)
                .map_err(|e| ProvisionError::Backend(format!("read {}: {e}", path.display())))?;
            let m = parse(&path.display().to_string(), &bytes)?;
            self.services.insert(m.id.clone(), m);
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&ServiceManifest> {
        self.services.get(id)
    }

    pub fn list(&self) -> Vec<&ServiceManifest> {
        self.services.values().collect()
    }

    /// Validate a request spec carries the manifest's required fields.
    pub fn validate_spec(&self, id: &str, spec: &serde_json::Value) -> Result<(), ProvisionError> {
        let m = self
            .get(id)
            .ok_or_else(|| ProvisionError::Unsupported(id.to_string()))?;
        for field in &m.required_fields {
            let present = spec
                .get(field)
                .map(|v| !v.is_null() && v.as_str() != Some(""))
                .unwrap_or(false);
            if !present {
                return Err(ProvisionError::InvalidSpec(format!(
                    "service '{id}' requires field '{field}'"
                )));
            }
        }
        Ok(())
    }
}

fn parse(path: &str, bytes: &[u8]) -> Result<ServiceManifest, ProvisionError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ProvisionError::InvalidSpec(format!("{path}: not utf-8: {e}")))?;
    let m: ServiceManifest = serde_yaml::from_str(text)
        .map_err(|e| ProvisionError::InvalidSpec(format!("{path}: {e}")))?;
    m.validate()?;
    Ok(m)
}

fn collect_manifests(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), ProvisionError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_manifests(&path, out)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("service.yaml") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads_all_seeds() {
        let cat = ServiceCatalog::embedded().unwrap();
        for id in [
            "s3-bucket",
            "dynamodb-table",
            "ecr-repository",
            "ec2-instance",
            "ecs-task",
            "ecs-service",
            "alb",
            "secretsmanager-secret",
            "random-secret",
            "rds-postgres",
            "tf-demo",
            "exec-echo",
            "databricks",
            "databricks-sql-warehouse",
            "databricks-job",
            "databricks-model-serving",
            "databricks-uc-volume",
            "litellm-key",
        ] {
            assert!(cat.get(id).is_some(), "missing seed manifest {id}");
        }
        // Per-project LiteLLM key: governed credential (human review), litellm
        // connector + litellm cost source. Also proves the A0 const allow-lists.
        let lk = cat.get("litellm-key").unwrap();
        assert!(!lk.auto_approvable);
        assert_eq!(lk.connector(), "litellm");
        assert_eq!(lk.cost.source.source_type, "litellm");
        // Databricks inference is a plug-in manifest (openai-compatible), not core
        // code — same shape as LiteLLM, just a different request path.
        let dbx = cat.get("databricks").unwrap();
        assert_eq!(dbx.connector(), "stub");
        let inf = dbx.inference.as_ref().unwrap();
        assert_eq!(inf.kind, "openai-compatible");
        assert_eq!(
            inf.chat_path.as_deref(),
            Some("/serving-endpoints/{model}/invocations")
        );
        // Cost-bearing services are self-service by default; the classification
        // floor + budget caps gate them, not a blanket review. Only credential-
        // minting (litellm-key) keeps a hard human gate.
        assert!(cat.get("databricks-sql-warehouse").unwrap().auto_approvable);
        assert!(cat.get("databricks-model-serving").unwrap().auto_approvable);
        assert!(cat.get("databricks-uc-volume").unwrap().auto_approvable);
        let s3 = cat.get("s3-bucket").unwrap();
        assert_eq!(s3.connector(), "terraform");
        assert_eq!(s3.cost.estimated_monthly_usd, 5.0);
        assert!(s3.auto_approvable);
        assert!(cat.get("rds-postgres").unwrap().auto_approvable);
        // Cost-bearing primitives are self-service; budget + classification gate them.
        assert!(cat.get("ecs-service").unwrap().auto_approvable);
        assert!(cat.get("alb").unwrap().auto_approvable);
        let ecs = cat.get("ecs-service").unwrap();
        assert_eq!(ecs.connector(), "terraform");
        assert!(ecs.required_fields.contains(&"vpc_id".to_string()));
        assert!(ecs.required_fields.contains(&"subnet_ids".to_string()));
    }

    // Terraform `module` paths must be relative to `modules_dir` (the modules-tree
    // root, e.g. `aws/ecs-service`). A repo-root `modules/` prefix double-counts
    // under the documented `ASGARD_TF_MODULES_DIR=/modules` (→ `/modules/modules/…`)
    // and 500s every provision.
    #[test]
    fn terraform_module_paths_are_relative_to_modules_dir() {
        fn assert_relative(id: &str, module: &str) {
            assert!(
                !module.starts_with('/'),
                "service '{id}': module '{module}' must be relative to modules_dir, not absolute"
            );
            assert!(
                !module.starts_with("modules/") && !module.starts_with("./modules/"),
                "service '{id}': module '{module}' must not carry a repo-root 'modules/' prefix — \
                 it resolves against modules_dir (the modules-tree root)"
            );
        }
        let cat = ServiceCatalog::embedded().unwrap();
        for m in cat.list() {
            // A terraform service may omit config.module when the module is supplied
            // per-request (e.g. access-grant uses the target's grant.module).
            if m.connector() == "terraform" {
                if let Some(module) = m.connector_config().get("module").and_then(|v| v.as_str()) {
                    assert_relative(&m.id, module);
                }
            }
            // Per-target grant modules resolve the same way and need the same shape.
            if let Some(g) = &m.grant {
                assert_relative(&m.id, &g.module);
            }
        }
    }

    #[test]
    fn validate_spec_enforces_required_fields() {
        let cat = ServiceCatalog::embedded().unwrap();
        assert!(cat
            .validate_spec("dynamodb-table", &serde_json::json!({"name": "t"}))
            .is_err());
        assert!(cat
            .validate_spec(
                "dynamodb-table",
                &serde_json::json!({"name": "t", "pk_name": "id"})
            )
            .is_ok());
    }

    #[test]
    fn overlay_dir_adds_and_overrides_services() {
        let dir = std::env::temp_dir().join(format!(
            "asgard-overlay-{}",
            crate::secrets::random_secret()
        ));
        let svc = dir.join("custom-svc");
        std::fs::create_dir_all(&svc).unwrap();
        // A brand-new service (no recompile) ...
        std::fs::write(
            svc.join("service.yaml"),
            b"id: custom-svc\nname: Custom\ncategory: tooling\nprovisioner: { connector: exec }\ncost: { model: free, source: { type: none } }\n",
        )
        .unwrap();
        // ... and an override of an embedded one (bump the estimate).
        let over = dir.join("s3");
        std::fs::create_dir_all(&over).unwrap();
        std::fs::write(
            over.join("service.yaml"),
            b"id: s3-bucket\nname: S3\ncategory: storage\nprovisioner: { connector: terraform }\ncost: { model: usage, estimated_monthly_usd: 99.0, source: { type: aws-cost-explorer } }\n",
        )
        .unwrap();

        let cat = ServiceCatalog::load(Some(&dir)).unwrap();
        assert!(
            cat.get("custom-svc").is_some(),
            "overlay service not loaded"
        );
        assert_eq!(
            cat.get("s3-bucket").unwrap().cost.estimated_monthly_usd,
            99.0
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_connector() {
        let bad = br#"
id: bad
name: Bad
category: tooling
provisioner: { connector: nope }
cost: { model: free, source: { type: none } }
"#;
        assert!(parse("bad.yaml", bad).is_err());
    }
}
