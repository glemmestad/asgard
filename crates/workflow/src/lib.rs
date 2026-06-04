//! Request → approval → fulfillment (brief §4.3). A small state machine backed by
//! the store, integrated with the policy engine: on submit, policy decides
//! whether the request is auto-approved, needs approval (and from whom, via an
//! obligation), or is denied outright. Every transition is audited.

use std::sync::Arc;

use asgard_policy::{PolicyEngine, Request as PolicyRequest};
use asgard_storage::audit::{self, AuditRecord};
use asgard_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("storage: {0}")]
    Storage(#[from] asgard_storage::StorageError),
    #[error("request not found: {0}")]
    NotFound(String),
    #[error("invalid transition from {from} to {to}")]
    InvalidTransition { from: String, to: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Requested,
    Approved,
    Rejected,
    Fulfilled,
    Cancelled,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Requested => "requested",
            State::Approved => "approved",
            State::Rejected => "rejected",
            State::Fulfilled => "fulfilled",
            State::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> State {
        match s {
            "approved" => State::Approved,
            "rejected" => State::Rejected,
            "fulfilled" => State::Fulfilled,
            "cancelled" => State::Cancelled,
            _ => State::Requested,
        }
    }
}

fn can_transition(from: State, to: State) -> bool {
    use State::*;
    matches!(
        (from, to),
        (Requested, Approved)
            | (Requested, Rejected)
            | (Requested, Cancelled)
            | (Approved, Fulfilled)
            | (Approved, Cancelled)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRequest {
    pub id: String,
    pub kind: String,
    pub requester: String,
    pub subject: String,
    pub state: State,
    pub approver: Option<String>,
    pub payload: serde_json::Value,
    pub reason: Option<String>,
    pub sla_due_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct NewRequest {
    pub kind: String,
    pub requester: String,
    pub subject: String,
    pub payload: serde_json::Value,
    pub sla_seconds: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct RequestFilter {
    pub state: Option<State>,
    pub requester: Option<String>,
    pub limit: Option<i64>,
}

pub struct WorkflowEngine {
    db: Db,
    policy: Arc<dyn PolicyEngine>,
}

impl WorkflowEngine {
    pub fn new(db: Db, policy: Arc<dyn PolicyEngine>) -> Self {
        WorkflowEngine { db, policy }
    }

    fn action_for(kind: &str) -> &'static str {
        match kind {
            "decommission" => "decommission",
            "access" | "invoke" => "invoke",
            "promotion" => "promote",
            "review-extension" => "extend",
            "budget" => "set_budget",
            _ => "deploy",
        }
    }

    /// Submit a request. Policy decides the initial state:
    /// denied → `Rejected`; allowed with approval obligation → `Requested`;
    /// allowed outright → `Approved`.
    pub async fn submit(&self, new: NewRequest) -> Result<WorkflowRequest, WorkflowError> {
        let decision = self
            .policy
            .is_authorized(&PolicyRequest::new(
                &new.requester,
                Self::action_for(&new.kind),
                &new.subject,
                new.payload.clone(),
            ))
            .await;

        let (state, approver, reason) = if !decision.allowed() {
            (State::Rejected, None, Some(decision.reasons.join("; ")))
        } else if let Some(approver) = decision.requires_approval() {
            (State::Requested, Some(approver.to_string()), None)
        } else {
            (
                State::Approved,
                None,
                Some("auto-approved: no approval required".to_string()),
            )
        };

        let now = asgard_storage::now();
        let sla_due_at = new.sla_seconds.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });

        let req = WorkflowRequest {
            id: asgard_storage::new_uid(),
            kind: new.kind,
            requester: new.requester,
            subject: new.subject,
            state,
            approver,
            payload: new.payload,
            reason,
            sla_due_at,
            created_at: now.clone(),
            updated_at: now,
        };

        sqlx::query(&self.db.q(
            "INSERT INTO workflow_requests (id, kind, requester, subject, state, approver, payload, reason, sla_due_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&req.id)
        .bind(&req.kind)
        .bind(&req.requester)
        .bind(&req.subject)
        .bind(req.state.as_str())
        .bind(&req.approver)
        .bind(req.payload.to_string())
        .bind(&req.reason)
        .bind(&req.sla_due_at)
        .bind(&req.created_at)
        .bind(&req.updated_at)
        .execute(self.db.pool())
        .await?;

        self.audit(&req, "workflow.submitted", &req.requester).await;
        Ok(req)
    }

    pub async fn get(&self, id: &str) -> Result<Option<WorkflowRequest>, WorkflowError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, kind, requester, subject, state, approver, payload, reason, sla_due_at, created_at, updated_at \
             FROM workflow_requests WHERE id = ?",
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(row_to_request).transpose()
    }

    pub async fn list(
        &self,
        filter: &RequestFilter,
    ) -> Result<Vec<WorkflowRequest>, WorkflowError> {
        let mut sql = String::from(
            "SELECT id, kind, requester, subject, state, approver, payload, reason, sla_due_at, created_at, updated_at \
             FROM workflow_requests WHERE 1=1",
        );
        if filter.state.is_some() {
            sql.push_str(" AND state = ?");
        }
        if filter.requester.is_some() {
            sql.push_str(" AND requester = ?");
        }
        sql.push_str(" ORDER BY created_at DESC");
        sql.push_str(&format!(" LIMIT {}", filter.limit.unwrap_or(500)));

        let sql = self.db.q(&sql);
        let mut q = sqlx::query(&sql);
        if let Some(s) = filter.state {
            q = q.bind(s.as_str());
        }
        if let Some(r) = &filter.requester {
            q = q.bind(r);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        rows.into_iter().map(row_to_request).collect()
    }

    pub async fn approve(
        &self,
        id: &str,
        approver: &str,
        reason: Option<&str>,
    ) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(id, State::Approved, Some(approver), reason, approver)
            .await
    }

    pub async fn reject(
        &self,
        id: &str,
        approver: &str,
        reason: Option<&str>,
    ) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(id, State::Rejected, Some(approver), reason, approver)
            .await
    }

    pub async fn fulfill(&self, id: &str, actor: &str) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(id, State::Fulfilled, None, None, actor)
            .await
    }

    pub async fn cancel(&self, id: &str, actor: &str) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(
            id,
            State::Cancelled,
            None,
            Some("cancelled by requester"),
            actor,
        )
        .await
    }

    async fn transition(
        &self,
        id: &str,
        to: State,
        approver: Option<&str>,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<WorkflowRequest, WorkflowError> {
        let mut req = self
            .get(id)
            .await?
            .ok_or_else(|| WorkflowError::NotFound(id.to_string()))?;
        if !can_transition(req.state, to) {
            return Err(WorkflowError::InvalidTransition {
                from: req.state.as_str().to_string(),
                to: to.as_str().to_string(),
            });
        }
        let now = asgard_storage::now();
        req.state = to;
        if let Some(a) = approver {
            req.approver = Some(a.to_string());
        }
        if let Some(r) = reason {
            req.reason = Some(r.to_string());
        }
        req.updated_at = now.clone();

        sqlx::query(&self.db.q(
            "UPDATE workflow_requests SET state = ?, approver = ?, reason = ?, updated_at = ? WHERE id = ?",
        ))
        .bind(req.state.as_str())
        .bind(&req.approver)
        .bind(&req.reason)
        .bind(&now)
        .bind(&req.id)
        .execute(self.db.pool())
        .await?;

        self.audit(&req, &format!("workflow.{}", to.as_str()), actor)
            .await;
        Ok(req)
    }

    async fn audit(&self, req: &WorkflowRequest, action: &str, actor: &str) {
        let rec = AuditRecord::new(actor, action)
            .entity(&req.subject)
            .outcome(req.state.as_str())
            .reason(req.reason.clone().unwrap_or_default())
            .data(serde_json::json!({"request_id": req.id, "kind": req.kind}));
        let _ = audit::append(&self.db, &rec).await;
    }
}

fn row_to_request(row: sqlx::any::AnyRow) -> Result<WorkflowRequest, WorkflowError> {
    let payload_str: String = row.get("payload");
    Ok(WorkflowRequest {
        id: row.get("id"),
        kind: row.get("kind"),
        requester: row.get("requester"),
        subject: row.get("subject"),
        state: State::parse(&row.get::<String, _>("state")),
        approver: row.get("approver"),
        payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
        reason: row.get("reason"),
        sla_due_at: row.get("sla_due_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asgard_policy::CedarEngine;

    async fn engine() -> WorkflowEngine {
        let path = std::env::temp_dir().join(format!("asgard-wf-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        WorkflowEngine::new(db, Arc::new(CedarEngine::new().unwrap()))
    }

    #[tokio::test]
    async fn restricted_deploy_needs_approval_then_fulfills() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "deploy".into(),
                requester: "user:default/alice".into(),
                subject: "agent:default/triage".into(),
                payload: serde_json::json!({"data_class": "restricted"}),
                sla_seconds: Some(86400),
            })
            .await
            .unwrap();
        assert_eq!(r.state, State::Requested);
        assert_eq!(r.approver.as_deref(), Some("group:default/security"));
        assert!(r.sla_due_at.is_some());

        let approved = e
            .approve(&r.id, "user:default/seclead", Some("ok"))
            .await
            .unwrap();
        assert_eq!(approved.state, State::Approved);

        let fulfilled = e.fulfill(&r.id, "system").await.unwrap();
        assert_eq!(fulfilled.state, State::Fulfilled);
    }

    #[tokio::test]
    async fn non_restricted_deploy_auto_approved() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "deploy".into(),
                requester: "user:default/alice".into(),
                subject: "agent:default/triage".into(),
                payload: serde_json::json!({"data_class": "internal"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        assert_eq!(r.state, State::Approved);
    }

    #[tokio::test]
    async fn denied_request_is_rejected() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "invoke".into(),
                requester: "user:default/alice".into(),
                subject: "model:default/gpt".into(),
                payload: serde_json::json!({"data_class": "restricted", "model_data_classes": ["public"]}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        assert_eq!(r.state, State::Rejected);
    }

    #[tokio::test]
    async fn invalid_transition_errors() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "deploy".into(),
                requester: "user:default/alice".into(),
                subject: "agent:default/x".into(),
                payload: serde_json::json!({"data_class": "restricted"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        // Requested -> Fulfilled is not allowed (must be approved first).
        let err = e.fulfill(&r.id, "system").await;
        assert!(matches!(err, Err(WorkflowError::InvalidTransition { .. })));
    }
}
