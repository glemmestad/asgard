//! Request → approval → fulfillment (brief §4.3). A small state machine backed by
//! the store, integrated with the policy engine: on submit, policy decides
//! whether the request is auto-approved, needs approval (and from whom, via an
//! obligation), or is denied outright. Every transition is audited.

use std::sync::Arc;

use frontkeep_policy::{PolicyEngine, Request as PolicyRequest};
use frontkeep_storage::audit::{self, AuditRecord};
use frontkeep_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("storage: {0}")]
    Storage(#[from] frontkeep_storage::StorageError),
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
    /// Parked in an async background review (a `code-review` reviewer reads the
    /// repo). A transient state the worker resolves to `Approved`/`Requested`
    /// (clean) or `Flagged` (findings); survives a restart via the `review_jobs`
    /// queue, not memory.
    Reviewing,
    /// Blocked by review findings and returned to the submitter to fix-and-retry
    /// or escalate. A resting state owned by the submitter, not yet a human's
    /// queue — distinct from `Requested` (pending a human approver).
    Flagged,
    Approved,
    Rejected,
    Fulfilled,
    Cancelled,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Requested => "requested",
            State::Reviewing => "reviewing",
            State::Flagged => "flagged",
            State::Approved => "approved",
            State::Rejected => "rejected",
            State::Fulfilled => "fulfilled",
            State::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> State {
        match s {
            "reviewing" => State::Reviewing,
            "flagged" => State::Flagged,
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
            | (Requested, Flagged)
            | (Requested, Reviewing)
            | (Approved, Reviewing)
            | (Reviewing, Approved)
            | (Reviewing, Requested)
            | (Reviewing, Flagged)
            | (Reviewing, Cancelled)
            | (Flagged, Requested)
            | (Flagged, Approved)
            | (Flagged, Cancelled)
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

impl WorkflowRequest {
    /// The project this request concerns, if any. Provision and budget requests carry
    /// it in the payload (`project_id`); promotion requests encode it as a
    /// `project:<id>` subject. Used to route approval authority to the project's
    /// manager regardless of how the request type names its subject.
    pub fn project_id(&self) -> Option<&str> {
        if let Some(p) = self.payload.get("project_id").and_then(|v| v.as_str()) {
            return Some(p);
        }
        self.subject.strip_prefix("project:")
    }
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
    pub subject: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Clone)]
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

        let now = frontkeep_storage::now();
        let sla_due_at = new.sla_seconds.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });

        let req = WorkflowRequest {
            id: frontkeep_storage::new_uid(),
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
        if filter.subject.is_some() {
            sql.push_str(" AND subject = ?");
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
        if let Some(s) = &filter.subject {
            q = q.bind(s);
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
        // Idempotent: the async provisioning worker and the reconciler can both
        // drive the same request to completion, so a second fulfill is a no-op
        // rather than an InvalidTransition (Fulfilled has no outgoing edge).
        if let Some(req) = self.get(id).await? {
            if req.state == State::Fulfilled {
                return Ok(req);
            }
        }
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

    /// Return a request to the submitter (review findings to fix-or-escalate). The
    /// approver group Cedar assigned is left intact for a later [`escalate`].
    pub async fn flag(
        &self,
        id: &str,
        actor: &str,
        summary: &str,
    ) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(id, State::Flagged, None, Some(summary), actor)
            .await
    }

    /// Submitter forwards a flagged request to its human approver.
    pub async fn escalate(&self, id: &str, actor: &str) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(
            id,
            State::Requested,
            None,
            Some("escalated to human review by submitter"),
            actor,
        )
        .await
    }

    /// Park a promotion in async background review. The Cedar-assigned approver is
    /// preserved (`None`) so a clean verdict can restore the human queue.
    pub async fn review(&self, id: &str, actor: &str) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(
            id,
            State::Reviewing,
            None,
            Some("under automated code review"),
            actor,
        )
        .await
    }

    /// Resolve a `Reviewing` promotion to its post-review resting state on a clean
    /// verdict: the pre-review state the caller stashed (`Approved` for a clean
    /// Light auto-approve, `Requested` for a Wide+ awaiting a human). The approver
    /// is preserved. Findings take the `flag` path instead, not this one.
    pub async fn resolve_review(
        &self,
        id: &str,
        to: State,
        reason: &str,
        actor: &str,
    ) -> Result<WorkflowRequest, WorkflowError> {
        self.transition(id, to, None, Some(reason), actor).await
    }

    /// Replace a request's payload (no state change). Used to stash the pre-review
    /// baseline before parking in `Reviewing`, and to write the reviewer summary
    /// once the async worker finishes. Not audited (informational fields only).
    pub async fn update_payload(
        &self,
        id: &str,
        payload: serde_json::Value,
    ) -> Result<(), WorkflowError> {
        let now = frontkeep_storage::now();
        sqlx::query(
            &self
                .db
                .q("UPDATE workflow_requests SET payload = ?, updated_at = ? WHERE id = ?"),
        )
        .bind(payload.to_string())
        .bind(&now)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
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
        let now = frontkeep_storage::now();
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
    use frontkeep_policy::CedarEngine;

    async fn engine() -> WorkflowEngine {
        let path =
            std::env::temp_dir().join(format!("frontkeep-wf-{}.db", frontkeep_storage::new_uid()));
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
    async fn flag_then_escalate_then_fulfill() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "promotion".into(),
                requester: "user:default/alice".into(),
                subject: "project:proj-2026-0001".into(),
                payload: serde_json::json!({"target_classification": "wide-operational"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        assert_eq!(r.state, State::Requested);
        let approver = r.approver.clone();

        // Findings → returned to submitter; approver preserved for escalation.
        let flagged = e.flag(&r.id, "system", "fix the eval url").await.unwrap();
        assert_eq!(flagged.state, State::Flagged);
        assert_eq!(flagged.approver, approver);
        assert_eq!(flagged.reason.as_deref(), Some("fix the eval url"));

        // Submitter escalates → human queue.
        let escalated = e.escalate(&r.id, "user:default/alice").await.unwrap();
        assert_eq!(escalated.state, State::Requested);

        let approved = e.approve(&r.id, "user:default/plat", None).await.unwrap();
        assert_eq!(approved.state, State::Approved);
        let fulfilled = e.fulfill(&r.id, "system").await.unwrap();
        assert_eq!(fulfilled.state, State::Fulfilled);
    }

    #[tokio::test]
    async fn admin_authorizes_flagged_directly_and_subject_filter() {
        let e = engine().await;
        let r = e
            .submit(NewRequest {
                kind: "promotion".into(),
                requester: "user:default/alice".into(),
                subject: "project:proj-2026-0002".into(),
                payload: serde_json::json!({"target_classification": "wide-operational"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        e.flag(&r.id, "system", "concern").await.unwrap();
        // Admin override: Flagged → Approved directly.
        let approved = e
            .approve(&r.id, "user:default/admin", Some("override"))
            .await
            .unwrap();
        assert_eq!(approved.state, State::Approved);

        let by_subject = e
            .list(&RequestFilter {
                subject: Some("project:proj-2026-0002".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_subject.len(), 1);
        assert_eq!(by_subject[0].id, r.id);
    }

    #[tokio::test]
    async fn reviewing_resolves_clean_or_flags() {
        let e = engine().await;
        // Clean Wide promotion (needs a human): Requested → Reviewing → Requested,
        // preserving the approver across the round trip.
        let r = e
            .submit(NewRequest {
                kind: "promotion".into(),
                requester: "user:default/alice".into(),
                subject: "project:proj-2026-0009".into(),
                payload: serde_json::json!({"target_classification": "wide-operational"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        assert_eq!(r.state, State::Requested);
        let approver = r.approver.clone();
        let reviewing = e.review(&r.id, "system").await.unwrap();
        assert_eq!(reviewing.state, State::Reviewing);
        assert_eq!(reviewing.approver, approver);
        let restored = e
            .resolve_review(&r.id, State::Requested, "review passed", "system")
            .await
            .unwrap();
        assert_eq!(restored.state, State::Requested);
        assert_eq!(restored.approver, approver);

        // Findings → Reviewing → Flagged.
        let r2 = e
            .submit(NewRequest {
                kind: "promotion".into(),
                requester: "user:default/alice".into(),
                subject: "project:proj-2026-0010".into(),
                payload: serde_json::json!({"target_classification": "wide-operational"}),
                sla_seconds: None,
            })
            .await
            .unwrap();
        e.review(&r2.id, "system").await.unwrap();
        let flagged = e
            .flag(&r2.id, "system", "repo violates standards")
            .await
            .unwrap();
        assert_eq!(flagged.state, State::Flagged);
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
