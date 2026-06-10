//! Authorization for Frontkeep (RFC-0002 Part A). One `PolicyEngine` trait queried
//! by gateway, catalog, workflow, and runtime. Cedar is the in-tree default;
//! `PolicyEngine` is the seam an OPA/Rego backend would implement.

mod cedar_engine;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use cedar_engine::CedarEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
}

/// A condition attached to an allowed decision (e.g. needs approval first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Obligation {
    RequiresApproval { approver: String },
}

#[derive(Debug, Clone)]
pub struct Request {
    /// EntityRef of the actor, e.g. `user:default/alice`.
    pub principal: String,
    /// `deploy` | `invoke` | `read` | `decommission` | `approve`.
    pub action: String,
    /// EntityRef of the target, e.g. `model:default/gpt-4o`.
    pub resource: String,
    /// Decision-relevant facts (data_class, model_data_classes, project_killed,
    /// budget_exceeded, is_owner, is_approver, ...).
    pub context: serde_json::Value,
}

impl Request {
    pub fn new(
        principal: impl Into<String>,
        action: impl Into<String>,
        resource: impl Into<String>,
        context: serde_json::Value,
    ) -> Self {
        Request {
            principal: principal.into(),
            action: action.into(),
            resource: resource.into(),
            context,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub effect: Effect,
    /// Determining policy ids / human-readable reasons (for audit).
    pub reasons: Vec<String>,
    pub obligations: Vec<Obligation>,
}

impl Decision {
    pub fn allowed(&self) -> bool {
        self.effect == Effect::Allow
    }

    pub fn deny(reason: impl Into<String>) -> Decision {
        Decision {
            effect: Effect::Deny,
            reasons: vec![reason.into()],
            obligations: vec![],
        }
    }

    pub fn requires_approval(&self) -> Option<&str> {
        self.obligations
            .iter()
            .map(|Obligation::RequiresApproval { approver }| approver.as_str())
            .next()
    }
}

#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn is_authorized(&self, req: &Request) -> Decision;
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy parse error: {0}")]
    Parse(String),
    #[error("context error: {0}")]
    Context(String),
    #[error("request error: {0}")]
    Request(String),
}
