//! Classification promotion — the pure evaluator behind the tier gate.
//!
//! The governance policy moves a project between tiers on explicit triggers,
//! with promotion *machine-gated on evidence* and routed to a human only on
//! exception. This module is that machine: given a project's evidence record
//! (WS1) and a target tier, it computes which required fields are missing and
//! which exception signals fire — no DB, no policy engine, fully unit-testable.
//! The workflow + Cedar layer turns the verdict into an auto-approve or a routed
//! review; this module never mutates anything.
//!
//! Structure is in code (the four tiers, the evaluator mechanism); the
//! *requirement table* and thresholds are the org-specific mutable layer
//! ([`ClassificationRequirements`], operator-overridable in `frontkeep.yaml`,
//! mirroring `GroupAllowlist`'s empty=default posture).

use std::collections::BTreeMap;

use crate::{Registration, RegistryError, CLASSIFICATIONS};
use serde::Serialize;

/// Rank of a classification within the ordered tier ladder, or `None` if unknown.
pub fn tier_rank(classification: &str) -> Option<usize> {
    CLASSIFICATIONS.iter().position(|c| *c == classification)
}

/// The tier exactly one step above `classification`, or `None` at the top / for
/// an unknown tier.
pub fn next_tier(classification: &str) -> Option<&'static str> {
    let rank = tier_rank(classification)?;
    CLASSIFICATIONS.get(rank + 1).copied()
}

/// One-step-up only. A two-step jump, a non-move, or a downward target via the
/// promotion path is rejected (demotion is a separate, explicit, reason-required
/// action).
pub fn validate_step(current: &str, target: &str) -> Result<(), RegistryError> {
    let cur = tier_rank(current)
        .ok_or_else(|| RegistryError::Validation(format!("unknown classification '{current}'")))?;
    let tgt = tier_rank(target)
        .ok_or_else(|| RegistryError::Validation(format!("unknown classification '{target}'")))?;
    if tgt == cur + 1 {
        Ok(())
    } else {
        Err(RegistryError::Validation(format!(
            "promotion must be exactly one step up (from '{current}' the only target is '{}')",
            next_tier(current).unwrap_or("<none: already at top tier>")
        )))
    }
}

/// The per-tier required-field table. `light`/`wide`/`critical` hold the fields
/// that become mandatory *at* that tier (cumulative: a wide target requires the
/// light list plus the wide list). Seeded from the policy doc's Classification
/// Evidence Record; an operator overrides any tier's list in `frontkeep.yaml`.
#[derive(Debug, Clone)]
pub struct ClassificationRequirements {
    light: Vec<String>,
    wide: Vec<String>,
    critical: Vec<String>,
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for ClassificationRequirements {
    fn default() -> Self {
        ClassificationRequirements {
            light: owned(&[
                "repo_or_source_url",
                "business_owner",
                "technical_owner",
                "team_or_org_of_record",
                "support_contact",
                "runbook_url",
                "monitoring_or_logs_url",
                "ci_status_url",
                "primary_data_flows",
                "critical_flow_test_or_eval_url",
                "state_loss_posture",
                "requested_classification",
            ]),
            wide: owned(&[
                "security_review_status",
                "architecture_summary_url",
                "critical_dependencies",
                "incident_path",
                "slo_or_service_target",
                "rpo_rto",
                "decommission_path",
            ]),
            critical: owned(&[
                "executive_accountable_owner",
                "risk_acceptance_url",
                "dr_exercise_evidence_url",
                "audit_retention_requirement",
                "recurring_review_date",
            ]),
        }
    }
}

impl ClassificationRequirements {
    /// Resolve operator overrides over the shipped default: any tier present in
    /// `overrides` replaces that tier's list; absent tiers keep the default
    /// (mirrors `GroupAllowlist`'s empty=open posture, per-tier). Unknown tier
    /// keys are ignored.
    pub fn from_overrides(overrides: BTreeMap<String, Vec<String>>) -> Self {
        let mut reqs = ClassificationRequirements::default();
        for (tier, fields) in overrides {
            match tier.as_str() {
                "light-operational" => reqs.light = fields,
                "wide-operational" => reqs.wide = fields,
                "critical-path" => reqs.critical = fields,
                _ => {}
            }
        }
        reqs
    }

    /// The cumulative required-field set for a target tier (the target's list
    /// plus every lower tier's). POC requires nothing beyond the core dims.
    pub fn required_through(&self, target: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(rank) = tier_rank(target) {
            if rank >= 1 {
                out.extend(self.light.iter().cloned());
            }
            if rank >= 2 {
                out.extend(self.wide.iter().cloned());
            }
            if rank >= 3 {
                out.extend(self.critical.iter().cloned());
            }
        }
        out
    }
}

/// The machine verdict for a promotion to `target`. `evidence_complete` gates
/// auto-approval; `exception_signals` force a human even when evidence is
/// complete; `unverified_signals` are the policy's exception triggers Frontkeep has
/// no data source for yet (surfaced honestly rather than silently passed).
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceVerdict {
    pub target: String,
    pub evidence_complete: bool,
    pub missing: Vec<String>,
    pub exception_signals: Vec<String>,
    pub unverified_signals: Vec<String>,
}

impl EvidenceVerdict {
    /// No missing evidence and no exception signal — the only state in which the
    /// policy permits a machine auto-approve (and only for a Light target; Wide+
    /// always route to a human regardless, enforced in Cedar).
    pub fn auto_approvable(&self) -> bool {
        self.evidence_complete && self.exception_signals.is_empty()
    }
}

/// The self-serve view an owner sees before requesting a promotion: where the
/// project is, the one tier it may move to, and the evidence verdict for that
/// move. `next_tier`/`verdict` are absent only when already at the top tier.
#[derive(Debug, Clone, Serialize)]
pub struct PromotionChecklist {
    pub current: String,
    pub next_tier: Option<String>,
    pub verdict: Option<EvidenceVerdict>,
}

/// Exception signals the policy lists but Frontkeep cannot machine-check: they need
/// usage/exposure telemetry or an out-of-band human report that the control
/// plane doesn't have. Emitted on every verdict so the gate is honest about what
/// it didn't verify rather than implying a clean pass.
const UNVERIFIED_SIGNALS: &[&str] = &[
    "requested tier may be below observed usage/exposure/cost (no telemetry source)",
    "public / partner / regulated access may be involved (no exposure field)",
    "a human may have reported a concern (no report channel)",
];

/// Evaluate a project's readiness to promote to `target` against the resolved
/// requirement table. Pure: reads only the registration record.
pub fn evaluate(
    reg: &Registration,
    target: &str,
    reqs: &ClassificationRequirements,
) -> EvidenceVerdict {
    let required = reqs.required_through(target);
    let missing: Vec<String> = required
        .iter()
        .filter(|f| !field_present(reg, f))
        .cloned()
        .collect();
    let evidence_complete = missing.is_empty();

    let mut exception_signals = Vec::new();
    if !evidence_complete {
        exception_signals.push("required evidence missing or empty".to_string());
    }
    // Sensitive data without a matching security review.
    let sensitive = matches!(reg.data_class.as_str(), "confidential" | "restricted");
    let reviewed = matches!(
        reg.evidence.security_review_status.as_str(),
        "approved" | "waived"
    );
    if sensitive && !reviewed {
        exception_signals.push(format!(
            "data_class '{}' present without an approved/waived security review (status: '{}')",
            reg.data_class,
            if reg.evidence.security_review_status.is_empty() {
                "not declared"
            } else {
                &reg.evidence.security_review_status
            }
        ));
    }
    // An unsupported-stack exception is, by definition, an exception.
    if !reg.evidence.stack_exception.trim().is_empty() {
        exception_signals.push("a stack exception is declared".to_string());
    }
    // No credible owner / team / support / decommission path at the target's
    // required level. owner is a core dim (always checked); the others only count
    // once the target tier makes them mandatory.
    let required_set: std::collections::HashSet<&str> =
        required.iter().map(String::as_str).collect();
    let mut absent_paths = Vec::new();
    if reg.owner.trim().is_empty() {
        absent_paths.push("owner");
    }
    for f in [
        "team_or_org_of_record",
        "support_contact",
        "decommission_path",
    ] {
        if required_set.contains(f) && !field_present(reg, f) {
            absent_paths.push(f);
        }
    }
    if !absent_paths.is_empty() {
        exception_signals.push(format!(
            "no credible accountability path: {} empty",
            absent_paths.join(", ")
        ));
    }

    EvidenceVerdict {
        target: target.to_string(),
        evidence_complete,
        missing,
        exception_signals,
        unverified_signals: UNVERIFIED_SIGNALS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Presence check for a required field by name: non-empty trimmed string, or
/// non-empty list. Maps the policy's field names onto the evidence record (WS1)
/// plus the core registration dims. Unknown names are treated as absent.
fn field_present(reg: &Registration, field: &str) -> bool {
    let ev = &reg.evidence;
    let s = |v: &str| !v.trim().is_empty();
    match field {
        // Core registration dims.
        "owner" => s(&reg.owner),
        "manager" => s(&reg.manager),
        "group" => s(&reg.group),
        "data_class" => s(&reg.data_class),
        "description" => s(&reg.description),
        // Evidence — single-value.
        "requested_classification" => s(&ev.requested_classification),
        "repo_or_source_url" => s(&ev.repo_or_source_url),
        "business_owner" => s(&ev.business_owner),
        "technical_owner" => s(&ev.technical_owner),
        "team_or_org_of_record" => s(&ev.team_or_org_of_record),
        "support_contact" => s(&ev.support_contact),
        "runbook_url" => s(&ev.runbook_url),
        "monitoring_or_logs_url" => s(&ev.monitoring_or_logs_url),
        "ci_status_url" => s(&ev.ci_status_url),
        "critical_flow_test_or_eval_url" => s(&ev.critical_flow_test_or_eval_url),
        "state_loss_posture" => s(&ev.state_loss_posture),
        "stack_exception" => s(&ev.stack_exception),
        "security_review_status" => s(&ev.security_review_status),
        "architecture_summary_url" => s(&ev.architecture_summary_url),
        "incident_path" => s(&ev.incident_path),
        "slo_or_service_target" => s(&ev.slo_or_service_target),
        "rpo_rto" => s(&ev.rpo_rto),
        "decommission_path" => s(&ev.decommission_path),
        "executive_accountable_owner" => s(&ev.executive_accountable_owner),
        "risk_acceptance_url" => s(&ev.risk_acceptance_url),
        "dr_exercise_evidence_url" => s(&ev.dr_exercise_evidence_url),
        "audit_retention_requirement" => s(&ev.audit_retention_requirement),
        "recurring_review_date" => s(&ev.recurring_review_date),
        // Evidence — multi-value (present = non-empty list).
        "maintainers" => !ev.maintainers.is_empty(),
        "critical_dependencies" => !ev.critical_dependencies.is_empty(),
        "primary_data_flows" => !ev.primary_data_flows.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Evidence;

    fn reg_at(classification: &str) -> Registration {
        Registration {
            project_id: "proj-2026-0001".into(),
            name: "Test".into(),
            owner: "alice@corp.example".into(),
            manager: "bob@corp.example".into(),
            group: "platform".into(),
            cost_center: "CC-100".into(),
            classification: classification.into(),
            data_class: "internal".into(),
            budget_usd: 0.0,
            spent_usd: 0.0,
            lifecycle: "active".into(),
            registered: true,
            killed: false,
            description: "d".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            review_date: String::new(),
            review_state: "ok".into(),
            review_extensions: 0,
            stack_exception_renewal_date: String::new(),
            evidence: Evidence::default(),
        }
    }

    /// A registration whose evidence satisfies the full Light requirement set.
    fn light_ready() -> Registration {
        let mut r = reg_at("poc");
        let ev = &mut r.evidence;
        ev.repo_or_source_url = "https://git".into();
        ev.business_owner = "biz@corp.example".into();
        ev.technical_owner = "tech@corp.example".into();
        ev.team_or_org_of_record = "Platform".into();
        ev.support_contact = "oncall@corp.example".into();
        ev.runbook_url = "https://runbook".into();
        ev.monitoring_or_logs_url = "https://logs".into();
        ev.ci_status_url = "N/A: no CI yet".into();
        ev.primary_data_flows = vec!["s3 -> warehouse".into()];
        ev.critical_flow_test_or_eval_url = "https://eval".into();
        ev.state_loss_posture = "stateless".into();
        ev.requested_classification = "light-operational".into();
        r
    }

    #[test]
    fn step_validation_one_up_only() {
        assert!(validate_step("poc", "light-operational").is_ok());
        assert!(validate_step("light-operational", "wide-operational").is_ok());
        // Two-step jump rejected.
        assert!(validate_step("poc", "wide-operational").is_err());
        // Non-move rejected.
        assert!(validate_step("poc", "poc").is_err());
        // Downward via promotion path rejected.
        assert!(validate_step("wide-operational", "poc").is_err());
        // Unknown tier rejected.
        assert!(validate_step("poc", "bogus").is_err());
    }

    #[test]
    fn next_tier_walks_the_ladder() {
        assert_eq!(next_tier("poc"), Some("light-operational"));
        assert_eq!(next_tier("wide-operational"), Some("critical-path"));
        assert_eq!(next_tier("critical-path"), None);
    }

    #[test]
    fn light_clean_pass_auto_approvable() {
        let reqs = ClassificationRequirements::default();
        let v = evaluate(&light_ready(), "light-operational", &reqs);
        assert!(v.evidence_complete, "missing: {:?}", v.missing);
        assert!(v.exception_signals.is_empty(), "{:?}", v.exception_signals);
        assert!(v.auto_approvable());
        // Honest about what it can't check.
        assert_eq!(v.unverified_signals.len(), 3);
    }

    #[test]
    fn light_missing_field_blocks() {
        let reqs = ClassificationRequirements::default();
        let mut r = light_ready();
        r.evidence.runbook_url = String::new();
        let v = evaluate(&r, "light-operational", &reqs);
        assert!(!v.evidence_complete);
        assert!(v.missing.contains(&"runbook_url".to_string()));
        assert!(!v.auto_approvable());
        assert!(v
            .exception_signals
            .iter()
            .any(|s| s.contains("missing or empty")));
    }

    #[test]
    fn sensitive_data_without_review_is_an_exception() {
        let reqs = ClassificationRequirements::default();
        let mut r = light_ready();
        r.data_class = "restricted".into();
        let v = evaluate(&r, "light-operational", &reqs);
        // Evidence itself is complete, but the data class forces a human.
        assert!(v.evidence_complete);
        assert!(!v.auto_approvable());
        assert!(v
            .exception_signals
            .iter()
            .any(|s| s.contains("security review")));
        // A waiver clears it.
        r.evidence.security_review_status = "waived".into();
        let v2 = evaluate(&r, "light-operational", &reqs);
        assert!(v2.auto_approvable());
    }

    #[test]
    fn stack_exception_forces_human() {
        let reqs = ClassificationRequirements::default();
        let mut r = light_ready();
        r.evidence.stack_exception = "uses an unsupported runtime".into();
        let v = evaluate(&r, "light-operational", &reqs);
        assert!(v.evidence_complete);
        assert!(!v.auto_approvable());
        assert!(v
            .exception_signals
            .iter()
            .any(|s| s.contains("stack exception")));
    }

    #[test]
    fn wide_requires_more_than_light() {
        let reqs = ClassificationRequirements::default();
        // A Light-ready project is not Wide-ready: the wide-only fields are absent.
        let v = evaluate(&light_ready(), "wide-operational", &reqs);
        assert!(!v.evidence_complete);
        assert!(v.missing.contains(&"architecture_summary_url".to_string()));
        assert!(v.missing.contains(&"decommission_path".to_string()));
    }

    #[test]
    fn critical_requires_risk_acceptance_field() {
        let reqs = ClassificationRequirements::default();
        let v = evaluate(&reg_at("wide-operational"), "critical-path", &reqs);
        assert!(v.missing.contains(&"risk_acceptance_url".to_string()));
        assert!(v
            .missing
            .contains(&"executive_accountable_owner".to_string()));
    }

    #[test]
    fn operator_override_replaces_one_tier() {
        let mut over = BTreeMap::new();
        over.insert(
            "light-operational".to_string(),
            vec!["runbook_url".to_string()],
        );
        let reqs = ClassificationRequirements::from_overrides(over);
        // Only runbook_url is now required for Light; the rest of the default
        // light list no longer gates.
        let mut r = reg_at("poc");
        r.evidence.runbook_url = "https://rb".into();
        let v = evaluate(&r, "light-operational", &reqs);
        assert!(v.evidence_complete, "missing: {:?}", v.missing);
        // Wide tier is untouched by the override.
        assert_eq!(reqs.required_through("wide-operational").len(), 1 + 7);
    }
}
