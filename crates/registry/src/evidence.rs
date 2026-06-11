//! Classification evidence record — the policy's machine-checkable fields beyond
//! the core registration dims. All fields are optional at this layer; tier
//! requirements are enforced by the promotion flow, not here. Stored on
//! `projects_runtime` (the system of record) rather than on the gateway's lean
//! `ProjectRuntime`, since these are governance metadata, not hot-path state.

use crate::RegistryError;
use frontkeep_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

pub const SECURITY_REVIEW_STATUSES: &[&str] = &[
    "not-started",
    "requested",
    "in-review",
    "approved",
    "waived",
];
pub const STATE_LOSS_POSTURES: &[&str] = &["stateless", "recoverable", "durable-critical"];

/// The evidence record, flattened into the project intake/record wire types so
/// the JSON stays a flat field set (matching the schema and UI) while Rust keeps
/// one struct. Multi-value fields persist as JSON-text columns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Evidence {
    #[serde(default)]
    pub requested_classification: String,
    #[serde(default)]
    pub repo_or_source_url: String,
    #[serde(default)]
    pub business_owner: String,
    #[serde(default)]
    pub technical_owner: String,
    #[serde(default)]
    pub team_or_org_of_record: String,
    #[serde(default)]
    pub support_contact: String,
    #[serde(default)]
    pub runbook_url: String,
    #[serde(default)]
    pub monitoring_or_logs_url: String,
    #[serde(default)]
    pub ci_status_url: String,
    #[serde(default)]
    pub critical_flow_test_or_eval_url: String,
    #[serde(default)]
    pub state_loss_posture: String,
    #[serde(default)]
    pub stack_exception: String,
    #[serde(default)]
    pub security_review_status: String,
    #[serde(default)]
    pub architecture_summary_url: String,
    #[serde(default)]
    pub incident_path: String,
    #[serde(default)]
    pub slo_or_service_target: String,
    #[serde(default)]
    pub rpo_rto: String,
    #[serde(default)]
    pub decommission_path: String,
    #[serde(default)]
    pub executive_accountable_owner: String,
    #[serde(default)]
    pub risk_acceptance_url: String,
    #[serde(default)]
    pub dr_exercise_evidence_url: String,
    #[serde(default)]
    pub audit_retention_requirement: String,
    #[serde(default)]
    pub recurring_review_date: String,
    #[serde(default)]
    pub maintainers: Vec<String>,
    #[serde(default)]
    pub critical_dependencies: Vec<String>,
    #[serde(default)]
    pub primary_data_flows: Vec<String>,
}

const SELECT_COLS: &str = "requested_classification, repo_or_source_url, business_owner, \
     technical_owner, team_or_org_of_record, support_contact, runbook_url, \
     monitoring_or_logs_url, ci_status_url, critical_flow_test_or_eval_url, \
     state_loss_posture, stack_exception, security_review_status, \
     architecture_summary_url, incident_path, slo_or_service_target, rpo_rto, \
     decommission_path, executive_accountable_owner, risk_acceptance_url, \
     dr_exercise_evidence_url, audit_retention_requirement, recurring_review_date, \
     maintainers, critical_dependencies, primary_data_flows";

impl Evidence {
    /// Reject enum fields whose value is non-empty and not in the allowed set.
    pub fn validate(&self) -> Result<(), RegistryError> {
        check_enum(
            "security_review_status",
            &self.security_review_status,
            SECURITY_REVIEW_STATUSES,
        )?;
        check_enum(
            "state_loss_posture",
            &self.state_loss_posture,
            STATE_LOSS_POSTURES,
        )?;
        Ok(())
    }

    /// Ordered (column, value) pairs for an UPDATE; vecs are JSON-encoded.
    fn columns(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "requested_classification",
                self.requested_classification.clone(),
            ),
            ("repo_or_source_url", self.repo_or_source_url.clone()),
            ("business_owner", self.business_owner.clone()),
            ("technical_owner", self.technical_owner.clone()),
            ("team_or_org_of_record", self.team_or_org_of_record.clone()),
            ("support_contact", self.support_contact.clone()),
            ("runbook_url", self.runbook_url.clone()),
            (
                "monitoring_or_logs_url",
                self.monitoring_or_logs_url.clone(),
            ),
            ("ci_status_url", self.ci_status_url.clone()),
            (
                "critical_flow_test_or_eval_url",
                self.critical_flow_test_or_eval_url.clone(),
            ),
            ("state_loss_posture", self.state_loss_posture.clone()),
            ("stack_exception", self.stack_exception.clone()),
            (
                "security_review_status",
                self.security_review_status.clone(),
            ),
            (
                "architecture_summary_url",
                self.architecture_summary_url.clone(),
            ),
            ("incident_path", self.incident_path.clone()),
            ("slo_or_service_target", self.slo_or_service_target.clone()),
            ("rpo_rto", self.rpo_rto.clone()),
            ("decommission_path", self.decommission_path.clone()),
            (
                "executive_accountable_owner",
                self.executive_accountable_owner.clone(),
            ),
            ("risk_acceptance_url", self.risk_acceptance_url.clone()),
            (
                "dr_exercise_evidence_url",
                self.dr_exercise_evidence_url.clone(),
            ),
            (
                "audit_retention_requirement",
                self.audit_retention_requirement.clone(),
            ),
            ("recurring_review_date", self.recurring_review_date.clone()),
            ("maintainers", encode_list(&self.maintainers)),
            (
                "critical_dependencies",
                encode_list(&self.critical_dependencies),
            ),
            ("primary_data_flows", encode_list(&self.primary_data_flows)),
        ]
    }
}

fn check_enum(field: &str, value: &str, allowed: &[&str]) -> Result<(), RegistryError> {
    if value.is_empty() || allowed.contains(&value) {
        Ok(())
    } else {
        Err(RegistryError::Validation(format!(
            "{field} must be one of: {}",
            allowed.join(", ")
        )))
    }
}

fn encode_list(items: &[String]) -> String {
    if items.is_empty() {
        String::new()
    } else {
        serde_json::to_string(items).unwrap_or_default()
    }
}

fn decode_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

/// Persist the evidence fields onto an existing `projects_runtime` row.
pub(crate) async fn write(db: &Db, project_id: &str, ev: &Evidence) -> Result<(), RegistryError> {
    let cols = ev.columns();
    let set = cols
        .iter()
        .map(|(c, _)| format!("{c} = ?"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = db.q(&format!(
        "UPDATE projects_runtime SET {set} WHERE project_id = ?"
    ));
    let mut q = sqlx::query(&sql);
    for (_, v) in &cols {
        q = q.bind(v);
    }
    q.bind(project_id).execute(db.pool()).await?;
    Ok(())
}

/// Read the evidence fields for a project (empty record if the row is absent).
pub(crate) async fn read(db: &Db, project_id: &str) -> Result<Evidence, RegistryError> {
    let sql = db.q(&format!(
        "SELECT {SELECT_COLS} FROM projects_runtime WHERE project_id = ?"
    ));
    let row = sqlx::query(&sql)
        .bind(project_id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(from_row).unwrap_or_default())
}

fn from_row(r: sqlx::any::AnyRow) -> Evidence {
    Evidence {
        requested_classification: r.get("requested_classification"),
        repo_or_source_url: r.get("repo_or_source_url"),
        business_owner: r.get("business_owner"),
        technical_owner: r.get("technical_owner"),
        team_or_org_of_record: r.get("team_or_org_of_record"),
        support_contact: r.get("support_contact"),
        runbook_url: r.get("runbook_url"),
        monitoring_or_logs_url: r.get("monitoring_or_logs_url"),
        ci_status_url: r.get("ci_status_url"),
        critical_flow_test_or_eval_url: r.get("critical_flow_test_or_eval_url"),
        state_loss_posture: r.get("state_loss_posture"),
        stack_exception: r.get("stack_exception"),
        security_review_status: r.get("security_review_status"),
        architecture_summary_url: r.get("architecture_summary_url"),
        incident_path: r.get("incident_path"),
        slo_or_service_target: r.get("slo_or_service_target"),
        rpo_rto: r.get("rpo_rto"),
        decommission_path: r.get("decommission_path"),
        executive_accountable_owner: r.get("executive_accountable_owner"),
        risk_acceptance_url: r.get("risk_acceptance_url"),
        dr_exercise_evidence_url: r.get("dr_exercise_evidence_url"),
        audit_retention_requirement: r.get("audit_retention_requirement"),
        recurring_review_date: r.get("recurring_review_date"),
        maintainers: decode_list(&r.get::<String, _>("maintainers")),
        critical_dependencies: decode_list(&r.get::<String, _>("critical_dependencies")),
        primary_data_flows: decode_list(&r.get::<String, _>("primary_data_flows")),
    }
}
