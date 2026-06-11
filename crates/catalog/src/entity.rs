//! The typed entity graph (RFC-0001). Manifests are the YAML-in-Git form; an
//! `Entity` is the reconciled, stored form. `spec` stays a raw JSON value and is
//! validated against the kind's JSON Schema rather than a per-kind Rust struct,
//! so new kinds need only a schema, not a code change.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::error::CatalogError;

pub const API_VERSION: &str = "frontkeep.dev/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Active,
    Decommissioned,
    Archived,
}

impl Lifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lifecycle::Active => "active",
            Lifecycle::Decommissioned => "decommissioned",
            Lifecycle::Archived => "archived",
        }
    }
    pub fn parse(s: &str) -> Lifecycle {
        match s {
            "decommissioned" => Lifecycle::Decommissioned,
            "archived" => Lifecycle::Archived,
            _ => Lifecycle::Active,
        }
    }
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Lifecycle::Active)
    }
}

fn default_namespace() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    #[serde(rename = "type")]
    pub rel_type: String,
    pub target: String,
}

/// The on-disk YAML form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(
        rename = "apiVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub api_version: Option<String>,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
}

impl Manifest {
    /// Reconstruct the full envelope as a JSON value for schema validation.
    pub fn as_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Where an entity was reconciled from.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    pub source_id: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub commit: Option<String>,
}

/// The reconciled, stored form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub uid: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: serde_json::Value,
    #[serde(default)]
    pub relations: Vec<Relation>,
    pub lifecycle: Lifecycle,
    #[serde(skip)]
    pub origin: Origin,
    pub content_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}

impl Entity {
    pub fn from_manifest(m: Manifest, origin: Origin) -> Entity {
        let hash = content_hash(&m.kind, &m.metadata, &m.spec, &m.relations);
        let now = frontkeep_storage::now();
        Entity {
            uid: frontkeep_storage::new_uid(),
            kind: m.kind,
            metadata: m.metadata,
            spec: m.spec,
            relations: m.relations,
            lifecycle: Lifecycle::Active,
            origin,
            content_hash: hash,
            created_at: now.clone(),
            updated_at: now.clone(),
            seen_at: now,
            deleted_at: None,
        }
    }

    /// Canonical EntityRef string `kind:namespace/name` (kind lowercased).
    pub fn entity_ref(&self) -> String {
        format!(
            "{}:{}/{}",
            self.kind.to_lowercase(),
            self.metadata.namespace,
            self.metadata.name
        )
    }
}

/// A parsed entity reference: `[kind:][namespace/]name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub kind: Option<String>,
    pub namespace: String,
    pub name: String,
}

impl EntityRef {
    pub fn parse(s: &str) -> Result<EntityRef, CatalogError> {
        let s = s.trim();
        let (kind, rest) = match s.split_once(':') {
            Some((k, r)) if !k.is_empty() => (Some(k.to_lowercase()), r),
            _ => (None, s),
        };
        let (namespace, name) = match rest.split_once('/') {
            Some((ns, nm)) => (ns.to_string(), nm.to_string()),
            None => (default_namespace(), rest.to_string()),
        };
        if name.is_empty() {
            return Err(CatalogError::BadRef(s.to_string()));
        }
        Ok(EntityRef {
            kind,
            namespace,
            name,
        })
    }

    pub fn canonical(&self) -> String {
        match &self.kind {
            Some(k) => format!("{}:{}/{}", k, self.namespace, self.name),
            None => format!("{}/{}", self.namespace, self.name),
        }
    }
}

fn content_hash(
    kind: &str,
    metadata: &Metadata,
    spec: &serde_json::Value,
    relations: &[Relation],
) -> String {
    // serde_json maps are BTreeMap-backed (sorted keys) by default, so this is
    // deterministic across runs; used only for change detection, not security.
    let canon = serde_json::to_string(&serde_json::json!({
        "kind": kind,
        "metadata": metadata,
        "spec": spec,
        "relations": relations,
    }))
    .unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_parsing() {
        let r = EntityRef::parse("agent:default/code-reviewer").unwrap();
        assert_eq!(r.kind.as_deref(), Some("agent"));
        assert_eq!(r.namespace, "default");
        assert_eq!(r.name, "code-reviewer");
        assert_eq!(r.canonical(), "agent:default/code-reviewer");

        let r = EntityRef::parse("platform").unwrap();
        assert_eq!(r.kind, None);
        assert_eq!(r.namespace, "default");
        assert_eq!(r.name, "platform");

        let r = EntityRef::parse("Group:ns1/team").unwrap();
        assert_eq!(r.kind.as_deref(), Some("group"));
        assert_eq!(r.namespace, "ns1");

        assert!(EntityRef::parse("agent:default/").is_err());
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        let m1 = Manifest {
            api_version: Some(API_VERSION.into()),
            kind: "Agent".into(),
            metadata: Metadata {
                name: "x".into(),
                namespace: "default".into(),
                ..Default::default()
            },
            spec: serde_json::json!({"owner":"group:default/p","model":"model:default/m"}),
            relations: vec![],
        };
        let h1 = content_hash(&m1.kind, &m1.metadata, &m1.spec, &m1.relations);
        let h2 = content_hash(&m1.kind, &m1.metadata, &m1.spec, &m1.relations);
        assert_eq!(h1, h2);

        let mut m2 = m1.clone();
        m2.spec = serde_json::json!({"owner":"group:default/q","model":"model:default/m"});
        let h3 = content_hash(&m2.kind, &m2.metadata, &m2.spec, &m2.relations);
        assert_ne!(h1, h3);
    }

    #[test]
    fn entity_from_manifest_sets_ref() {
        let m = Manifest {
            api_version: Some(API_VERSION.into()),
            kind: "Agent".into(),
            metadata: Metadata {
                name: "code-reviewer".into(),
                namespace: "default".into(),
                ..Default::default()
            },
            spec: serde_json::json!({}),
            relations: vec![],
        };
        let e = Entity::from_manifest(m, Origin::default());
        assert_eq!(e.entity_ref(), "agent:default/code-reviewer");
        assert_eq!(e.lifecycle, Lifecycle::Active);
        assert!(!e.uid.is_empty());
    }
}
