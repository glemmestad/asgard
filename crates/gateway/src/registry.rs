//! Model registry: maps a catalog model ref (or provider-native name) to its
//! provider, route, data-class allowlist, and cost rates. Seeded statically or
//! from `Model` entities in the catalog.

use std::collections::HashMap;

use crate::error::GatewayError;

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub model_ref: String,
    pub provider: String,
    pub route_model: String,
    pub data_classes: Vec<String>,
    pub cost_in: f64,
    pub cost_out: f64,
}

#[derive(Clone, Default)]
pub struct ModelRegistry {
    by_key: HashMap<String, ModelInfo>,
}

impl ModelRegistry {
    pub fn from_models(models: Vec<ModelInfo>) -> Self {
        let mut by_key = HashMap::new();
        for m in models {
            by_key.insert(m.model_ref.clone(), m.clone());
            by_key.entry(m.route_model.clone()).or_insert(m);
        }
        ModelRegistry { by_key }
    }

    pub fn resolve(&self, model: &str) -> Option<&ModelInfo> {
        self.by_key.get(model)
    }

    /// Add or overwrite a model (keyed by ref and route name).
    pub fn insert(&mut self, m: ModelInfo) {
        self.by_key.insert(m.model_ref.clone(), m.clone());
        self.by_key.entry(m.route_model.clone()).or_insert(m);
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Build from `Model` entities in the catalog.
    pub async fn from_catalog(repo: &frontkeep_catalog::CatalogRepo) -> Result<Self, GatewayError> {
        let filter = frontkeep_catalog::ListFilter {
            kind: Some("Model".to_string()),
            ..Default::default()
        };
        let entities = repo
            .list(&filter)
            .await
            .map_err(|e| GatewayError::Provider(format!("catalog: {e}")))?;
        let mut models = Vec::new();
        for e in entities {
            let spec = &e.spec;
            let provider = spec
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("mock")
                .to_string();
            let route_model = spec
                .get("route")
                .and_then(|v| v.as_str())
                .unwrap_or(&e.metadata.name)
                .to_string();
            let data_classes = spec
                .get("dataClassAllowlist")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let cost_in = spec
                .get("costPer1kIn")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let cost_out = spec
                .get("costPer1kOut")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            models.push(ModelInfo {
                model_ref: e.entity_ref(),
                provider,
                route_model,
                data_classes,
                cost_in,
                cost_out,
            });
        }
        Ok(ModelRegistry::from_models(models))
    }
}
