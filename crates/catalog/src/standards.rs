//! Published enterprise standards. The agent seed's `.agent/*.md` files ARE these
//! standards (embedded here at build time), so the seed an employee points an
//! agent at and the standards an agent fetches over MCP never drift. Served via
//! the `get_standards` / `list_standards` MCP tools and seeded as discoverable
//! `Standard` catalog entities.

use crate::entity::{Entity, Manifest, Metadata, Origin, API_VERSION};
use crate::error::CatalogError;
use crate::repo::CatalogRepo;

pub struct Standard {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
}

pub const STANDARDS: &[Standard] = &[
    Standard {
        id: "coding",
        title: "Engineering Standards",
        summary: "Code, testing, CI, conventional commits, dependency hygiene, documentation.",
        body: include_str!("../../../seed/.agent/STANDARDS.md"),
    },
    Standard {
        id: "security",
        title: "Security",
        summary: "Data classification, secrets, least privilege, the gateway, no shadow AI.",
        body: include_str!("../../../seed/.agent/SECURITY.md"),
    },
    Standard {
        id: "workflow",
        title: "Workflow",
        summary:
            "Branch, register (the gate), request resources via the control plane, eval/merge gate.",
        body: include_str!("../../../seed/.agent/WORKFLOW.md"),
    },
];

pub fn all() -> &'static [Standard] {
    STANDARDS
}

pub fn get(id: &str) -> Option<&'static Standard> {
    STANDARDS.iter().find(|s| s.id == id)
}

/// Upsert each standard as a discoverable `Standard` catalog entity.
pub async fn seed(catalog: &CatalogRepo) -> Result<(), CatalogError> {
    for s in STANDARDS {
        let manifest = Manifest {
            api_version: Some(API_VERSION.into()),
            kind: "Standard".into(),
            metadata: Metadata {
                name: s.id.into(),
                namespace: "default".into(),
                title: Some(s.title.into()),
                description: Some(s.summary.into()),
                ..Default::default()
            },
            spec: serde_json::json!({ "id": s.id, "summary": s.summary }),
            relations: vec![],
        };
        catalog
            .upsert(&Entity::from_manifest(manifest, Origin::default()))
            .await?;
    }
    Ok(())
}
