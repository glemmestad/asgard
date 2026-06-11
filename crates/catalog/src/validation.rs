//! JSON Schema validation for entity manifests (RFC-0001 §9). Schemas are
//! embedded at build time from `schemas/`. `Tool` and `MCPServer` share one
//! schema. Compilation happens per-validate to keep the registry trivially
//! `Send + Sync`; validation is an ingestion-time cost, not on any hot path.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::CatalogError;

const RAW: &[(&str, &str)] = &[
    ("Agent", include_str!("../../../schemas/agent.schema.json")),
    (
        "Prompt",
        include_str!("../../../schemas/prompt.schema.json"),
    ),
    ("Tool", include_str!("../../../schemas/mcp.schema.json")),
    (
        "MCPServer",
        include_str!("../../../schemas/mcp.schema.json"),
    ),
    ("Eval", include_str!("../../../schemas/eval.schema.json")),
    (
        "Dataset",
        include_str!("../../../schemas/dataset.schema.json"),
    ),
    (
        "Project",
        include_str!("../../../schemas/project.schema.json"),
    ),
];

#[derive(Clone)]
pub struct SchemaRegistry {
    schemas: HashMap<String, Value>,
}

impl SchemaRegistry {
    /// Build the registry from the schemas embedded at compile time.
    pub fn embedded() -> Result<SchemaRegistry, CatalogError> {
        let mut schemas = HashMap::new();
        for (kind, raw) in RAW {
            let v: Value = serde_json::from_str(raw)?;
            schemas.insert((*kind).to_string(), v);
        }
        Ok(SchemaRegistry { schemas })
    }

    pub fn known_kind(&self, kind: &str) -> bool {
        self.schemas.contains_key(kind)
    }

    /// Validate a manifest envelope value against its kind's schema. Returns the
    /// list of validation messages on failure.
    pub fn validate(&self, kind: &str, instance: &Value) -> Result<(), Vec<String>> {
        let schema = match self.schemas.get(kind) {
            Some(s) => s,
            None => return Err(vec![format!("no schema registered for kind '{kind}'")]),
        };
        let compiled = match jsonschema::JSONSchema::compile(schema) {
            Ok(c) => c,
            Err(e) => return Err(vec![format!("schema compile error: {e}")]),
        };
        let mut msgs = Vec::new();
        if let Err(errors) = compiled.validate(instance) {
            for e in errors {
                msgs.push(format!("{} (at {})", e, e.instance_path));
            }
        }
        if msgs.is_empty() {
            Ok(())
        } else {
            Err(msgs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_schemas_compile() {
        let reg = SchemaRegistry::embedded().unwrap();
        for kind in [
            "Agent",
            "Prompt",
            "Tool",
            "MCPServer",
            "Eval",
            "Dataset",
            "Project",
        ] {
            assert!(reg.known_kind(kind), "missing {kind}");
        }
    }

    #[test]
    fn valid_agent_passes_invalid_fails() {
        let reg = SchemaRegistry::embedded().unwrap();
        let good = serde_json::json!({
            "apiVersion": "frontkeep.dev/v1",
            "kind": "Agent",
            "metadata": {"name": "code-reviewer", "namespace": "default"},
            "spec": {"owner": "group:default/platform", "model": "model:default/gpt"}
        });
        assert!(reg.validate("Agent", &good).is_ok());

        // missing required spec.model
        let bad = serde_json::json!({
            "apiVersion": "frontkeep.dev/v1",
            "kind": "Agent",
            "metadata": {"name": "x"},
            "spec": {"owner": "group:default/platform"}
        });
        assert!(reg.validate("Agent", &bad).is_err());

        // wrong apiVersion const
        let bad2 = serde_json::json!({
            "apiVersion": "v2",
            "kind": "Agent",
            "metadata": {"name": "x"},
            "spec": {"owner": "group:default/p", "model": "model:default/m"}
        });
        assert!(reg.validate("Agent", &bad2).is_err());
    }
}
